//! Chunk-level recall over the resident's unified project view.
//!
//! CLI and desktop-worker callers use the same resident search path as agent
//! integrations, so they never open a competing writer beside the resident.

use crate::index::{DocRecord, SearchResult, SectionChunk};
#[cfg(test)]
use crate::integrations::mcp_index;
use crate::memory_api::MemoryService;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Instant;

/// The project index is shared with the MCP servers rather than kept under a
/// name of its own. Both paths build it from the same graph with the same
/// options, so a second copy would only double the disk cost and the rebuild
/// work each time the workspace moves on.
/// A single chunk of a long session memory can run to tens of kilobytes, and
/// every result crosses a gRPC boundary before anyone reads it, so the text is
/// cut to a window a reader (human or model) actually consumes. Cutting is done
/// by whole lines so the reported line range stays true to the text emitted.
const MAX_CHUNK_TEXT_BYTES: usize = 1_600;

/// Options for a one-shot recall over a project root.
///
/// Workspace scanning limits and ignore paths are retained for callers, but
/// indexing is resident-owned; recall reads the resident's published index.
#[derive(Debug, Clone)]
pub struct RecallOptions<'a> {
    pub root: &'a Path,
    pub query: &'a str,
    pub result_count: usize,
    pub ignore_paths: &'a [String],
    pub max_bytes: usize,
    pub max_files: usize,
    pub max_depth: usize,
    pub max_total_bytes: usize,
    pub memory_name: &'a str,
}
/// The wire contract consumed by lint-service: a `results` array whose every
/// element carries a numeric `score`. Fields beyond that pass through the
/// dispatcher untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallOutput {
    pub query: String,
    pub root: String,
    pub elapsed_ms: u128,
    pub results: Vec<RecallHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    pub score: f32,
    /// Documents keep their repo-relative path here; the dispatcher rewrites
    /// relative paths against the worker's base path, so they are never
    /// absolutised on this side. A memory is not a file, so it carries the
    /// corpus it lives in. The dispatcher turns that into a real absolute path
    /// too, which is what a reader on another machine can act on.
    pub doc_id: String,
    pub source: String,
    /// The record's own identity. It is kept out of `doc_id` because the
    /// dispatcher treats every relative-looking value there as a path and
    /// prefixes it, which turned `doc:2df221ab…` into a file that never existed.
    pub record_id: String,
    /// Where a memory came from, verbatim, for the same reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub kind: RecallKind,
    pub chunk: RecallChunk,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_status: Option<crate::semantic_relations::SemanticStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relation_evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecallKind {
    Document,
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallChunk {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    /// Null when the record carries no honest line numbers. A wrong range is
    /// worse than none: the desktop app jumps the editor to it.
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    /// True when the chunk was longer than the cap and only a window is here.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Query the unified store, including project documents and recorded memories,
/// and return chunk-level hits.
pub fn recall(options: &RecallOptions<'_>) -> Result<RecallOutput> {
    let root = options
        .root
        .canonicalize()
        .with_context(|| format!("unable to resolve project root {}", options.root.display()))?;

    // Indexing limits and ignores are resident-owned. Keep the legacy options
    // fields for callers while the resident remains the authoritative indexer.
    let _ = options;
    let client = crate::Memory::new(&root)
        .map_err(|error| anyhow::anyhow!("unable to connect to project resident: {error}"))?;
    let response = client
        .call_workspace_tool(
            &crate::agent::AgentOperationContext::new("codex"),
            "recall_search",
            serde_json::json!({
                "query": options.query,
                "result_count": options.result_count,
                "source": "all",
            }),
            false,
        )
        .map_err(|error| anyhow::anyhow!("resident recall search failed: {error}"))?;
    serde_json::from_value(response).context("resident returned an invalid recall response")
}

#[derive(Debug, Deserialize)]
struct RecallServerRequest {
    query: String,
    #[serde(default = "default_result_count")]
    result_count: usize,
    #[serde(default = "default_source", alias = "session_provider")]
    source: String,
}

fn default_result_count() -> usize {
    20
}
fn default_source() -> String {
    "documents".to_string()
}

/// Long-lived project recall. The expensive persistent stores are opened once;
/// each subsequent request only runs the in-memory query and response mapping.
pub fn run_recall_server(
    root: &Path,
    ignore_paths: &[String],
    max_bytes: usize,
    max_files: usize,
    max_depth: usize,
    max_total_bytes: usize,
) -> Result<()> {
    // The resident owns indexing configuration and project scanning limits.
    let _ = (
        ignore_paths,
        max_bytes,
        max_files,
        max_depth,
        max_total_bytes,
    );
    let root = root
        .canonicalize()
        .with_context(|| format!("unable to resolve project root {}", root.display()))?;
    let client = crate::Memory::new(&root)
        .map_err(|error| anyhow::anyhow!("unable to connect to project resident: {error}"))?;

    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "{{\"ready\":true}}")?;
    output.flush()?;

    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RecallServerRequest = serde_json::from_str(&line)?;
        let response = client
            .call_workspace_tool(
                &crate::agent::AgentOperationContext::new("codex"),
                "recall_search",
                serde_json::json!({
                    "query": request.query,
                    "result_count": request.result_count,
                    "source": request.source,
                }),
                false,
            )
            .map_err(|error| anyhow::anyhow!("resident recall search failed: {error}"))?;
        let response: RecallOutput = serde_json::from_value(response)
            .context("resident returned an invalid recall response")?;
        writeln!(output, "{}", serde_json::to_string(&response)?)?;
        output.flush()?;
    }
    Ok(())
}

/// Execute recall against the resident's read view. This path is called only
/// by the resident's private generic workspace operation, never by an SDK
/// process with a writable service.
pub(crate) fn recall_from_service(
    root: &str,
    query: &str,
    result_count: usize,
    source: &str,
    service: &MemoryService,
) -> Result<RecallOutput> {
    anyhow::ensure!(!query.trim().is_empty(), "query is required");
    anyhow::ensure!(result_count > 0, "result_count must be positive");
    anyhow::ensure!(
        matches!(source, "all" | "documents" | "claude" | "codex"),
        "source must be all, documents, claude, or codex"
    );
    let started = Instant::now();
    let results = service.query_plain_cached(query, result_count.saturating_mul(4))?;
    let mut hits = results
        .iter()
        .filter_map(|result| {
            let record = service.record_by_id(&result.doc_id)?;
            let is_memory = record.filters.contains_key("session_id")
                || record.filters.contains_key("document_type");
            if (source == "documents" && is_memory)
                || (matches!(source, "claude" | "codex") && !is_memory)
            {
                return None;
            }
            Some(hit_from(result, record, query))
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| right.score.total_cmp(&left.score));
    hits.truncate(result_count);
    Ok(RecallOutput {
        query: query.to_string(),
        root: root.to_string(),
        elapsed_ms: started.elapsed().as_millis(),
        results: hits,
    })
}

fn hit_from(result: &SearchResult, record: &DocRecord, query: &str) -> RecallHit {
    let filter = |key: &str| record.filters.get(key).cloned();
    // Only the integrations stamp these filters onto what they capture, so
    // their presence is what separates a recorded memory from a project file.
    let kind = if record.filters.contains_key("session_id")
        || record.filters.contains_key("document_type")
    {
        RecallKind::Memory
    } else {
        RecallKind::Document
    };
    let is_memory = kind == RecallKind::Memory;
    RecallHit {
        score: result.score,
        doc_id: if is_memory {
            memory_corpus_path(&record.source)
        } else {
            result.doc_id.clone()
        },
        source: if is_memory {
            memory_corpus_path(&record.source)
        } else {
            record.source.clone()
        },
        record_id: result.doc_id.clone(),
        origin: is_memory.then(|| record.source.clone()),
        kind,
        chunk: best_chunk(record, query, &result.matched_terms),
        session_id: filter("session_id"),
        branch: filter("branch"),
        revision: filter("revision"),
        document_type: filter("document_type"),
        timestamp: record.timestamp.clone(),
        semantic_status: result.semantic_status,
        superseded_by: result.superseded_by.clone(),
        relation_confidence: result.relation_confidence,
        relation_evidence: result.relation_evidence.clone(),
    }
}

/// Return the corpus path for a recorded memory. All providers share one
/// memory store; the provider is attribution on the record, not the path.
fn memory_corpus_path(_source: &str) -> String {
    format!(
        ".membrick/{}",
        crate::integrations::mcp_index::SHARED_MEMORY_DIR
    )
}

/// Pick the section chunk that best answers the query and bound its text.
fn best_chunk(record: &DocRecord, query: &str, matched_terms: &[String]) -> RecallChunk {
    let terms = excerpt_terms(query, matched_terms);
    let selected = record
        .section_chunks
        .iter()
        .max_by_key(|chunk| chunk_score(chunk, &terms));
    match selected {
        Some(chunk) => {
            let (content, first_line) =
                skip_recorder_header(&chunk.content, &terms, chunk.start_line.max(1));
            let (text, start_line, end_line, truncated) =
                bounded_window(content, first_line, &terms);
            RecallChunk {
                text,
                chunk_id: Some(chunk.chunk_id.clone()),
                start_line,
                end_line,
                truncated,
            }
        }
        // Chunking can legitimately produce nothing (an empty or unheaded
        // document). The record content still starts at line one, so a window
        // over it carries a range we can stand behind.
        None => {
            let (content, first_line) = skip_recorder_header(&record.content, &terms, 1);
            let (text, start_line, end_line, truncated) =
                bounded_window(content, first_line, &terms);
            RecallChunk {
                text,
                chunk_id: None,
                start_line,
                end_line,
                truncated,
            }
        }
    }
}

fn chunk_score(chunk: &SectionChunk, terms: &HashSet<String>) -> usize {
    let haystack = format!("{}\n{}", chunk.heading, chunk.content).to_ascii_lowercase();
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count()
}

/// Take a contiguous run of lines that fits the byte cap, centred on the
/// densest match, and report the line numbers that run really occupies.
/// The recorder writes a machine header above what it concluded. The agent
/// that wrote the memory and the type of the record. An excerpt that opens on
/// those lines spends its budget saying nothing, so leading header lines are
/// dropped when they carry none of what was asked about. The reported range
/// moves with them; a range that no longer matches its text would be worse than
/// the boilerplate.
fn skip_recorder_header<'a>(
    content: &'a str,
    terms: &HashSet<String>,
    first_line: usize,
) -> (&'a str, usize) {
    let mut offset = 0;
    let mut skipped = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        let is_header = trimmed.starts_with("Claude Code ")
            || trimmed.starts_with("Codex ")
            || trimmed.starts_with("Gemini CLI ")
            || trimmed.starts_with("Memory type:");
        if !is_header {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if terms.iter().any(|term| lower.contains(term.as_str())) {
            break;
        }
        offset += line.len() + 1;
        skipped += 1;
    }
    if offset >= content.len() {
        return (content, first_line);
    }
    (&content[offset..], first_line + skipped)
}

fn bounded_window(
    content: &str,
    first_line: usize,
    terms: &HashSet<String>,
) -> (String, Option<usize>, Option<usize>, bool) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return (String::new(), None, None, false);
    }
    // A stored chunk numbered from zero would push every reported range off by
    // one, so fall back to one-based numbering rather than emit a wrong range.
    let first_line = first_line.max(1);
    if content.len() <= MAX_CHUNK_TEXT_BYTES {
        return (
            content.trim_end().to_string(),
            Some(first_line),
            Some(first_line + lines.len() - 1),
            false,
        );
    }

    let best = lines
        .iter()
        .enumerate()
        .max_by_key(|(index, line)| {
            let lower = line.to_ascii_lowercase();
            let score = terms.iter().filter(|term| lower.contains(*term)).count();
            (score, std::cmp::Reverse(*index))
        })
        .map(|(index, _)| index)
        .unwrap_or(0);

    let mut start = best;
    let mut end = best;
    let mut used = lines[best].len();
    // Grow outwards a line at a time so the window stays contiguous, which is
    // what makes the reported range honest.
    loop {
        // Each neighbouring line costs its own bytes plus the newline the join
        // will put back between them.
        let after_cost = lines.get(end + 1).map(|line| line.len() + 1);
        let grew_after = matches!(after_cost, Some(cost) if used + cost <= MAX_CHUNK_TEXT_BYTES);
        if grew_after {
            end += 1;
            used += lines[end].len() + 1;
        }
        let before_cost = start.checked_sub(1).map(|index| lines[index].len() + 1);
        let grew_before = matches!(before_cost, Some(cost) if used + cost <= MAX_CHUNK_TEXT_BYTES);
        if grew_before {
            start -= 1;
            used += lines[start].len() + 1;
        }
        if !grew_after && !grew_before {
            break;
        }
    }

    let joined = lines[start..=end].join("\n");
    let joined_len = joined.len();
    let text = if joined_len > MAX_CHUNK_TEXT_BYTES {
        truncate_utf8(&joined, MAX_CHUNK_TEXT_BYTES.saturating_sub(3))
    } else {
        joined
    };
    let truncated = start > 0 || end + 1 < lines.len() || joined_len > MAX_CHUNK_TEXT_BYTES;
    (
        text.trim_end().to_string(),
        Some(first_line + start),
        Some(first_line + end),
        truncated,
    )
}

/// The terms an excerpt is scored against: what the user asked plus what the
/// index says actually matched.
pub(crate) fn excerpt_terms(query: &str, matched_terms: &[String]) -> HashSet<String> {
    let mut terms = query_terms(query);
    terms.extend(matched_terms.iter().flat_map(|term| query_terms(term)));
    terms
}

pub(crate) fn query_terms(value: &str) -> HashSet<String> {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "again", "also", "and", "are", "before", "from", "have", "into", "our",
        "that", "the", "their", "this", "was", "what", "when", "which", "with", "without",
    ];
    value
        .split(|character: char| {
            !character.is_alphanumeric() && character != '-' && character != '_'
        })
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 3 && !STOP_WORDS.contains(&term.as_str()))
        .collect()
}

#[allow(dead_code)]
pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", value[..end].trim_end())
}

/// The lines of `content` that carry the query, joined for injection into a
/// prompt. Unlike [`best_chunk`] this drops the gaps between the lines it
/// keeps, so it reports no line range.
#[allow(dead_code)]
pub(crate) fn relevant_excerpt(
    content: &str,
    query: &str,
    matched_terms: &[String],
    max_bytes: usize,
) -> String {
    // Term-ranked excerpts can keep the topic sentence and a closing
    // acknowledgment while dropping every numbered item. Preserve the list
    // as a unit when the query asks for an item or its order.
    if query_requests_list_item(query) {
        if let Some(list) = numbered_list_block(content) {
            if list.len() <= max_bytes {
                return list;
            }
        }
    }

    let terms = excerpt_terms(query, matched_terms);
    let mut lines = content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let lower = line.to_ascii_lowercase();
            let score = terms.iter().filter(|term| lower.contains(*term)).count();
            (index, score, line.trim())
        })
        .filter(|(_, _, line)| !line.is_empty())
        .collect::<Vec<_>>();
    lines.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));

    let mut selected = lines
        .iter()
        .filter(|(_, score, _)| *score > 0)
        .take(3)
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        selected.extend(lines.into_iter().take(2));
    }
    selected.sort_by_key(|(index, _, _)| *index);
    truncate_utf8(
        &selected
            .into_iter()
            .map(|(_, _, line)| line)
            .collect::<Vec<_>>()
            .join("\n"),
        max_bytes,
    )
}

fn query_requests_list_item(query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    if [
        "list", "numbered", "item", "entry", "order", "first", "second", "third",
    ]
    .iter()
    .any(|term| query.contains(term))
    {
        return true;
    }
    query
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            ["st", "nd", "rd", "th"].iter().any(|suffix| {
                token.strip_suffix(suffix).is_some_and(|number| {
                    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
                })
            })
        })
}

fn numbered_list_block(content: &str) -> Option<String> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut best: Option<(usize, usize)> = None;
    let mut start = None;
    let mut last_item = 0;
    let mut count = 0;
    for (index, line) in lines.iter().enumerate() {
        if is_numbered_list_item(line) {
            if start.is_none() {
                start = Some(index);
            }
            last_item = index;
            count += 1;
        } else if line.trim().is_empty() && start.is_some() {
            // Empty lines are often used to format Markdown lists. Keep
            // scanning; the final range includes them when rendering.
        } else if start.is_some() {
            if count >= 2 && best.is_none_or(|(_, best_count)| count > best_count) {
                best = Some((start.unwrap(), count));
            }
            start = None;
            count = 0;
        }
    }
    if start.is_some() && count >= 2 && best.is_none_or(|(_, best_count)| count > best_count) {
        best = Some((start.unwrap(), count));
    }
    let (start, _) = best?;
    let end = last_item.max(start);
    Some(lines[start..=end].join("\n"))
}

fn is_numbered_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].trim_start().starts_with(['.', ')'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Provenance, ScoreBreakdown};
    use crate::pipeline::{IndexStore, PipelineOptions};
    use crate::source::SourceDocument;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn provenance() -> Provenance {
        Provenance {
            source: "test".to_string(),
            timestamp: None,
            ner_provider: "heuristic".to_string(),
            term_ranker: "yake-style".to_string(),
            index_version: "test".to_string(),
        }
    }

    fn chunk(content: &str, start_line: usize, end_line: usize) -> SectionChunk {
        SectionChunk {
            chunk_id: "chunk:test".to_string(),
            heading: "(document)".to_string(),
            content: content.into(),
            start_line,
            end_line,
            timestamp: None,
            key_entities: Vec::new(),
            important_terms: Vec::new(),
        }
    }

    fn record(
        doc_id: &str,
        source: &str,
        content: &str,
        filters: BTreeMap<String, String>,
        section_chunks: Vec<SectionChunk>,
    ) -> DocRecord {
        DocRecord {
            doc_id: doc_id.to_string(),
            source: source.to_string(),
            content: content.into(),
            timestamp: Some("2026-08-21T23:46:12+00:00".to_string()),
            doc_length: content.len(),
            author_agent: None,
            group_id: None,
            filters,
            probable_topic: None,
            doc_type_guess: None,
            headings: Vec::new(),
            doc_links: Vec::new(),
            temporal_terms: Vec::new(),
            key_entities: Vec::new(),
            important_terms: Vec::new(),
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
            section_chunks,
            embedding: None,
            top_claims: Vec::new(),
            provenance: provenance(),
            content_hash: String::new(),
            semantic_tags: Vec::new(),
        }
    }

    fn search_result(doc_id: &str, source: &str, matched_terms: &[&str]) -> SearchResult {
        SearchResult {
            doc_id: doc_id.to_string(),
            source: source.to_string(),
            group_id: None,
            score: 88.4,
            score_breakdown: ScoreBreakdown::default(),
            matched_entities: Vec::new(),
            matched_terms: matched_terms.iter().map(|t| t.to_string()).collect(),
            probable_topic: None,
            doc_type_guess: None,
            semantic_status: None,
            superseded_by: None,
            relation_confidence: None,
            relation_evidence: Vec::new(),
            comparison_operands: Vec::new(),
        }
    }

    fn memory_filters() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("session_id".to_string(), "6cccf167".to_string()),
            ("branch".to_string(), "master".to_string()),
            ("revision".to_string(), "d4d78b7".to_string()),
            ("document_type".to_string(), "outcome".to_string()),
        ])
    }

    #[test]
    fn list_item_recall_keeps_the_numbered_list_in_order() {
        let content = "Saved conversation about work from home jobs for seniors.\n\n1. Virtual customer service representative\n2. Telehealth professional\n3. Remote bookkeeper\n4. Virtual tutor or teacher\n5. Freelance writer or editor\n6. Online survey taker\n7. Transcriptionist\n8. Social media manager\n\nThe assistant acknowledged the list.";
        let excerpt = relevant_excerpt(
            content,
            "what was the 7th job in the list of work from home jobs for seniors",
            &[
                "home".into(),
                "job".into(),
                "list".into(),
                "senior".into(),
                "work".into(),
            ],
            4_000,
        );

        assert!(excerpt.contains("7. Transcriptionist"), "{excerpt}");
        assert!(
            excerpt.contains("1. Virtual customer service representative"),
            "{excerpt}"
        );
        assert!(excerpt.contains("8. Social media manager"), "{excerpt}");
    }

    #[test]
    fn project_recall_sees_every_providers_memories_in_the_shared_store() {
        let providers = [
            "claude",
            "codex",
            "gemini-cli",
            "agy",
            "muse",
            "openclaw",
            "hermes",
        ];

        let temp_base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let root = temp_base.join(format!(
            "membrick-recall-shared-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("docs").join("architecture.md"),
            "# Architecture\nThe project architecture uses provider-aware recall.",
        )
        .unwrap();

        // Every provider writes into the same shared store, keeping its own
        // attribution on the documents.
        let memory_root = mcp_index::shared_memory_root(&root);
        let mut memory = IndexStore::at_path(&memory_root, PipelineOptions::default()).unwrap();
        for provider in providers {
            memory.upsert(SourceDocument {
                doc_id: format!("{provider}-session-1"),
                source: format!("{provider}://project/session-1/outcome"),
                content: "The provider-aware recall decision was recorded in this session.".into(),
                concept: "outcome".to_string(),
                group_id: Some(format!("{provider}-session:session-1")),
                filters: memory_filters(),
                headings: vec!["Outcome".to_string()],
                links: vec![],
                timestamp: Some("2026-08-22T00:00:00Z".to_string()),
                doc_length: 0,
                author_agent: Some(provider.to_string()),
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            });
        }
        memory.refresh().unwrap();
        drop(memory);

        let memory =
            MemoryService::at_path(&memory_root, mcp_index::segmented_store_options()).unwrap();
        let output = recall_from_service(
            &root.to_string_lossy(),
            "provider-aware recall",
            10,
            "all",
            &memory,
        )
        .unwrap();

        // The query path sees memories across providers: segment routing
        // (`query_top_n: 3`) picks the best segments, but the hits must span
        // more than one provider -- the old per-provider silo would only ever
        // return one.
        let hit_providers: std::collections::BTreeSet<&str> = output
            .results
            .iter()
            .filter(|hit| hit.kind == RecallKind::Memory)
            .filter_map(|hit| hit.origin.as_deref())
            .filter_map(|origin| origin.split("://").next())
            .collect();
        assert!(
            hit_providers.len() > 1,
            "recall did not span providers: {:?}",
            output.results
        );
        assert!(
            output.results.iter().all(|hit| {
                hit.kind != RecallKind::Memory
                    || hit.doc_id == format!(".membrick/{}", mcp_index::SHARED_MEMORY_DIR)
            }),
            "memory hits must come from the shared store: {:?}",
            output.results
        );

        // Deterministic: every provider's documents live in the one store.
        let store = IndexStore::at_path(&memory_root, PipelineOptions::default()).unwrap();
        for provider in providers {
            assert!(
                store
                    .source_document_by_id(&format!("{provider}-session-1"))
                    .is_some(),
                "{provider} memory missing from the shared store"
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_hit_carries_session_branch_and_revision() {
        let record = record(
            "doc:2df221abf669e91a",
            "claude-code://project/session/outcome",
            "we dropped embeddings",
            memory_filters(),
            vec![chunk("we dropped embeddings", 12, 12)],
        );
        let hit = hit_from(
            &search_result("doc:2df221abf669e91a", "claude-code://x", &["embeddings"]),
            &record,
            "why did we drop embeddings",
        );

        assert_eq!(hit.kind, RecallKind::Memory);
        assert_eq!(hit.session_id.as_deref(), Some("6cccf167"));
        assert_eq!(hit.branch.as_deref(), Some("master"));
        assert_eq!(hit.revision.as_deref(), Some("d4d78b7"));
        assert_eq!(hit.document_type.as_deref(), Some("outcome"));
        assert_eq!(hit.chunk.start_line, Some(12));
        assert!(hit.chunk.text.contains("embeddings"));
    }

    #[test]
    fn document_hit_carries_no_session_metadata() {
        let record = record(
            "docs/architecture.md",
            "docs/architecture.md",
            "# Architecture\nembeddings were dropped",
            BTreeMap::new(),
            vec![chunk("embeddings were dropped", 88, 88)],
        );
        let hit = hit_from(
            &search_result("docs/architecture.md", "docs/architecture.md", &[]),
            &record,
            "embeddings",
        );

        assert_eq!(hit.kind, RecallKind::Document);
        assert!(hit.session_id.is_none());
        assert!(hit.branch.is_none());
        assert!(hit.revision.is_none());
        // Repo-relative, because the dispatcher absolutises it against the
        // worker's base path.
        assert_eq!(hit.doc_id, "docs/architecture.md");
    }

    #[test]
    fn chunk_text_is_capped_and_the_reported_range_matches_it() {
        let mut lines: Vec<String> = (0..400)
            .map(|index| format!("filler line {index}"))
            .collect();
        lines[300] = "the verdict of the gauntlet critic".to_string();
        let body = lines.join("\n");
        let record = record(
            "doc:long",
            "claude-code://project/session/outcome",
            &body,
            memory_filters(),
            vec![chunk(&body, 1, 400)],
        );
        let hit = hit_from(
            &search_result("doc:long", "claude-code://x", &["verdict"]),
            &record,
            "gauntlet critic verdict",
        );

        assert!(hit.chunk.text.len() <= MAX_CHUNK_TEXT_BYTES);
        assert!(hit.chunk.truncated);
        assert!(hit.chunk.text.contains("verdict"));
        let start = hit.chunk.start_line.expect("a window has a start");
        let end = hit.chunk.end_line.expect("a window has an end");
        assert_eq!(end - start + 1, hit.chunk.text.lines().count());
        // The window must really sit where it says it does.
        assert_eq!(
            hit.chunk.text.lines().next(),
            body.lines().nth(start - 1),
            "reported start_line does not match the emitted text"
        );
    }

    #[test]
    fn a_single_long_line_is_still_bounded() {
        let body = format!("prefix {} suffix", "x".repeat(MAX_CHUNK_TEXT_BYTES * 2));
        let record = record(
            "doc:long-line",
            "claude-code://project/session/outcome",
            &body,
            memory_filters(),
            vec![chunk(&body, 7, 7)],
        );
        let hit = hit_from(
            &search_result("doc:long-line", "claude-code://x", &["prefix"]),
            &record,
            "prefix",
        );

        assert!(hit.chunk.text.len() <= MAX_CHUNK_TEXT_BYTES);
        assert!(hit.chunk.truncated);
        assert_eq!(hit.chunk.start_line, Some(7));
        assert_eq!(hit.chunk.end_line, Some(7));
    }

    #[test]
    fn a_record_without_chunks_still_reports_an_honest_range() {
        let record = record(
            "doc:unchunked",
            "claude-code://project/session/outcome",
            "first line\nsecond line",
            memory_filters(),
            Vec::new(),
        );
        let hit = hit_from(
            &search_result("doc:unchunked", "claude-code://x", &[]),
            &record,
            "second",
        );

        assert!(hit.chunk.chunk_id.is_none());
        assert_eq!(hit.chunk.start_line, Some(1));
        assert_eq!(hit.chunk.end_line, Some(2));
    }

    #[test]
    fn the_chunk_carrying_the_query_wins() {
        let record = record(
            "doc:multi",
            "claude-code://project/session/outcome",
            "unrelated\nembeddings were dropped",
            memory_filters(),
            vec![
                chunk("unrelated prose about packaging", 1, 1),
                chunk("embeddings were dropped", 2, 2),
            ],
        );
        let hit = hit_from(
            &search_result("doc:multi", "claude-code://x", &["embeddings"]),
            &record,
            "why did we drop embeddings",
        );

        assert_eq!(hit.chunk.start_line, Some(2));
        assert!(hit.chunk.text.contains("embeddings"));
    }
}
