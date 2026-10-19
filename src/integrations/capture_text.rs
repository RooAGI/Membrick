//! The text a capture keeps from a provider transcript or hook payload.
//!
//! A capture stores what was said in a session, not everything the session
//! carried. Every provider passes its transcript text through here, which
//! cuts down two kinds of content:
//!
//! - Memory context Membrick injected into the session. It is already stored,
//!   so capturing it again only copies earlier memories into new ones. Hooks
//!   wrap it in [`MEMORY_CONTEXT_START`] and [`MEMORY_CONTEXT_END`], and
//!   captures drop everything between them.
//! - Tool calls and their results. They keep the tool name and the start and
//!   end of their input and output, never the raw call records.
//!
//! Roo Runtime also marks a compaction summary it puts back into a prompt
//! with [`COMPACTION_SUMMARY_START`] and [`COMPACTION_SUMMARY_END`]. The
//! summary is stored once, when Roo reports the compaction, so captures drop
//! marked copies too.

use serde_json::Value;

pub(crate) const MEMORY_CONTEXT_START: &str = "<membrick-memory>";
pub(crate) const MEMORY_CONTEXT_END: &str = "</membrick-memory>";
pub(crate) const COMPACTION_SUMMARY_START: &str = "<roo-compaction-summary>";
pub(crate) const COMPACTION_SUMMARY_END: &str = "</roo-compaction-summary>";
pub(crate) const MAX_TOOL_INPUT_BYTES: usize = 1_000;
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 2_000;
pub(crate) const MAX_REASONING_BYTES: usize = 4_000;

/// Headers that began injected context before it was marked. Everything from
/// one of them to the end of the text is injected context.
const LEGACY_MEMORY_CONTEXT_HEADERS: [&str; 2] = [
    "Relevant project memory from Membrick:",
    "[membrick memory — recalled for this turn]",
];

/// Wrap retrieved memory so a later capture of the transcript can drop it.
pub(crate) fn mark_memory_context(context: &str) -> String {
    format!(
        "{MEMORY_CONTEXT_START}\n{}\n{MEMORY_CONTEXT_END}",
        context.trim_matches('\n')
    )
}

/// Remove injected memory context and repeated compaction summaries from
/// captured text.
pub(crate) fn strip_memory_context(text: &str) -> String {
    let mut kept = strip_blocks(
        &strip_blocks(text, MEMORY_CONTEXT_START, MEMORY_CONTEXT_END),
        COMPACTION_SUMMARY_START,
        COMPACTION_SUMMARY_END,
    );
    for header in LEGACY_MEMORY_CONTEXT_HEADERS {
        if let Some(start) = kept.find(header) {
            kept.truncate(start);
        }
    }
    if is_legacy_memory_entries(&kept) {
        return String::new();
    }
    kept
}

fn strip_blocks(text: &str, start_marker: &str, end_marker: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(start_marker) {
        kept.push_str(&rest[..start]);
        let block = &rest[start..];
        // An unterminated block was cut off mid-injection; drop the remainder.
        rest = block
            .find(end_marker)
            .map_or("", |end| &block[end + end_marker.len()..]);
    }
    kept.push_str(rest);
    kept
}

/// Injected context written before it was marked, as Claude Code and Codex
/// formatted it: a message made only of `- Source:` entries, each with
/// `Type:` and `Captured:` lines.
fn is_legacy_memory_entries(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("- Source: ") && text.contains("\n  Type: ") && text.contains("\n  Captured: ")
}

/// Keep the start and end of `text` within about `max_bytes`, marking what
/// was cut.
pub(crate) fn clip(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut head = max_bytes * 2 / 3;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (max_bytes - max_bytes * 2 / 3);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

/// A tool call as its name and the start and end of its input.
pub(crate) fn tool_call_text(name: &str, input: &Value) -> String {
    let input = match input {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    format!(
        "[tool call: {name}] {}",
        clip(input.trim(), MAX_TOOL_INPUT_BYTES)
    )
}

/// A tool result as the start and end of its output.
pub(crate) fn tool_result_text(output: &Value) -> String {
    format!(
        "[tool result] {}",
        clip(output_text(output).trim(), MAX_TOOL_OUTPUT_BYTES)
    )
}

fn output_text(output: &Value) -> String {
    match output {
        // Codex wraps some outputs in a JSON string with an `output` field.
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("output")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| output_text(block))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(fields) => fields
            .get("output")
            .or_else(|| fields.get("content"))
            .or_else(|| fields.get("text"))
            .map(output_text)
            .unwrap_or_else(|| serde_json::to_string(output).unwrap_or_default()),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The text of a message's `content`: a string, or the content blocks used by
/// Claude Code, Codex and OpenClaw messages.
pub(crate) fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => strip_memory_context(text),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(block_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn block_text(block: &Value) -> Option<String> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("content");
    let name = || {
        block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_string()
    };
    Some(match kind {
        "text" | "input_text" | "output_text" => {
            strip_memory_context(block.get("text").and_then(Value::as_str)?)
        }
        "tool_use" | "server_tool_use" | "toolCall" | "tool_call" => tool_call_text(
            &name(),
            block
                .get("input")
                .or_else(|| block.get("arguments"))
                .unwrap_or(&Value::Null),
        ),
        "tool_result" | "toolResult" | "web_search_tool_result" => strip_memory_context(
            &tool_result_text(block.get("content").unwrap_or(&Value::Null)),
        ),
        "thinking" => format!(
            "[thinking] {}",
            clip(
                block.get("thinking").and_then(Value::as_str)?.trim(),
                MAX_REASONING_BYTES
            )
        ),
        "redacted_thinking" => return None,
        "image" | "document" | "input_image" => format!("[{kind}]"),
        _ => format!(
            "[{kind}] {}",
            clip(
                &serde_json::to_string(block).unwrap_or_default(),
                MAX_TOOL_INPUT_BYTES
            )
        ),
    })
}

/// The text of a Codex `response_item` that is not a message: tool calls,
/// their outputs, and reasoning summaries. Encrypted reasoning is not kept.
pub(crate) fn response_item_text(item: &Value) -> Option<String> {
    let kind = item.get("type").and_then(Value::as_str)?;
    if kind.ends_with("_output") {
        return Some(strip_memory_context(&tool_result_text(
            item.get("output").unwrap_or(&Value::Null),
        )));
    }
    if kind == "reasoning" {
        let summary = item
            .get("summary")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return (!summary.trim().is_empty())
            .then(|| format!("[reasoning] {}", clip(summary.trim(), MAX_REASONING_BYTES)));
    }
    if kind.ends_with("_call") {
        let name = item.get("name").and_then(Value::as_str).unwrap_or(kind);
        let input = item
            .get("arguments")
            .or_else(|| item.get("input"))
            .or_else(|| item.get("action"))
            .unwrap_or(&Value::Null);
        return Some(tool_call_text(name, input));
    }
    Some(format!(
        "[{kind}] {}",
        clip(
            &serde_json::to_string(item).unwrap_or_default(),
            MAX_TOOL_INPUT_BYTES
        )
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn marked_memory_context_is_removed_and_surrounding_text_kept() {
        let text = format!(
            "Before\n{}\nAfter",
            mark_memory_context("\n- Source: codex://a/b/outcome\n  old memory\n")
        );
        assert_eq!(strip_memory_context(&text), "Before\n\nAfter");
    }

    #[test]
    fn legacy_injected_context_is_removed() {
        let codex = "\n- Source: codex://a/b/outcome\n  Type: outcome\n  Captured: now\n  x";
        assert_eq!(strip_memory_context(codex), "");
        let hermes = "Real question\n[membrick memory — recalled for this turn]\n- old";
        assert_eq!(strip_memory_context(hermes), "Real question\n");
        let plain = "- Source: a user list item that is not injected memory";
        assert_eq!(strip_memory_context(plain), plain);
    }

    #[test]
    fn clip_keeps_start_and_end_on_char_boundaries() {
        let text = "é".repeat(2_000);
        let clipped = clip(&text, 100);
        assert!(clipped.len() < 200);
        assert!(clipped.starts_with('é') && clipped.ends_with('é'));
        assert!(clipped.contains("bytes omitted"));
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn tool_blocks_keep_name_and_bounded_input_and_output() {
        let content = json!([
            {"type": "text", "text": "Let me look."},
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "x".repeat(5_000)}},
            {"type": "tool_result", "tool_use_id": "t1", "content": "y".repeat(50_000)},
            {"type": "thinking", "thinking": "why", "signature": "s".repeat(10_000)}
        ]);
        let text = content_text(&content);
        assert!(text.contains("Let me look."));
        assert!(text.contains("[tool call: Bash]"));
        assert!(text.contains("[tool result]"));
        assert!(text.contains("[thinking] why"));
        assert!(!text.contains(&"s".repeat(100)), "signatures are not kept");
        assert!(text.len() < 5_000, "got {} bytes", text.len());
    }

    #[test]
    fn codex_response_items_are_bounded() {
        let call = json!({"type": "custom_tool_call", "name": "exec", "input": "z".repeat(9_000), "call_id": "c"});
        let output = json!({"type": "custom_tool_call_output", "call_id": "c", "output": "o".repeat(90_000)});
        let reasoning = json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "e".repeat(9_000)});
        assert!(response_item_text(&call)
            .unwrap()
            .starts_with("[tool call: exec]"));
        assert!(response_item_text(&output).unwrap().len() < 2_200);
        assert_eq!(response_item_text(&reasoning).unwrap(), "[reasoning] plan");
    }
}
