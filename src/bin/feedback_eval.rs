//! Offline evaluation of previously submitted MCP search feedback.
//!
//! This binary does not import the Membrick library or open a workspace. It
//! reads the supplied telemetry files and writes page-ready dashboard
//! snapshots to the selected output directory.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const MAX_DASHBOARD_ROW_BYTES: usize = 512 * 1024;
const MAX_DASHBOARD_DOCUMENT_JUDGMENTS: usize = 50;

#[derive(Default)]
struct Counts {
    reviews: usize,
    joined_searches: usize,
    usable_searches: usize,
    judged_documents: usize,
    excluded_reviews: usize,
    malformed_history_lines: usize,
    overall_labels: BTreeMap<String, usize>,
    relevance_labels: BTreeMap<String, usize>,
}

fn usage() -> &'static str {
    "Usage: cargo run --features experimental --bin feedback_eval -- --history PATH --reviews PATH [--output-dir PATH] [--k 5,10]\n\nReads MCP history and search-review state files offline, then writes dashboard snapshots to the output directory (defaults to the review file's parent)."
}

fn arguments() -> Result<(PathBuf, PathBuf, PathBuf, Vec<usize>), String> {
    let mut history = None;
    let mut reviews = None;
    let mut output_dir = None;
    let mut cutoffs = vec![5, 10];
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--history" => history = args.next().map(PathBuf::from),
            "--reviews" => reviews = args.next().map(PathBuf::from),
            "--output-dir" => output_dir = args.next().map(PathBuf::from),
            "--k" => {
                let raw = args
                    .next()
                    .ok_or("--k requires comma-separated positive integers")?;
                cutoffs = raw
                    .split(',')
                    .map(|part| {
                        part.parse::<usize>()
                            .ok()
                            .filter(|value| *value > 0)
                            .ok_or_else(|| format!("invalid cutoff: {part}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                cutoffs.sort_unstable();
                cutoffs.dedup();
            }
            "-h" | "--help" => return Err(usage().to_string()),
            _ => return Err(format!("unknown argument: {arg}\n\n{}", usage())),
        }
    }
    let history = history.ok_or_else(|| format!("--history is required\n\n{}", usage()))?;
    let reviews = reviews.ok_or_else(|| format!("--reviews is required\n\n{}", usage()))?;
    let output_dir = output_dir.unwrap_or_else(|| {
        reviews
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf()
    });
    Ok((history, reviews, output_dir, cutoffs))
}

fn mcp_result(record: &Value) -> Option<Value> {
    let text = record
        .pointer("/response/result/content/0/text")?
        .as_str()?;
    serde_json::from_str(text).ok()
}

fn dashboard_document_judgment(judgment: &Value) -> Value {
    serde_json::json!({
        "doc_id": judgment.get("doc_id").and_then(Value::as_str).map(|id| truncate_utf8(id, 512)),
        "rank": judgment.get("rank").and_then(Value::as_u64),
        "source_hash": judgment.get("source_hash").and_then(Value::as_str).map(|hash| truncate_utf8(hash, 128)),
        "relevance": judgment.get("relevance").and_then(Value::as_str).map(|label| truncate_utf8(label, 64)),
        "feedback": judgment.get("feedback").and_then(Value::as_str).map(|text| truncate_utf8(text, 512)),
    })
}

fn dashboard_review_row(
    search_id: &str,
    submitted_at: u64,
    provider: Option<&Value>,
    query: &str,
    status: &str,
    label: Option<&Value>,
    feedback: Option<&Value>,
    judgments: &[Value],
) -> Value {
    serde_json::json!({
        "search_id": truncate_utf8(search_id, 256),
        "submitted_at_ms": submitted_at,
        "provider": provider.and_then(Value::as_str).map(|value| truncate_utf8(value, 128)),
        "query": truncate_utf8(query, 4096),
        "status": status,
        "label": label.and_then(Value::as_str).map(|value| truncate_utf8(value, 64)),
        "feedback": feedback.and_then(Value::as_str).map(|value| truncate_utf8(value, 8192)),
        "document_judgments": judgments
            .iter()
            .take(MAX_DASHBOARD_DOCUMENT_JUDGMENTS)
            .map(dashboard_document_judgment)
            .collect::<Vec<_>>(),
    })
}

fn relevance_gain(value: &str) -> Option<f64> {
    match value {
        "relevant" => Some(3.0),
        "partially_relevant" => Some(1.0),
        "not_relevant" => Some(0.0),
        _ => None,
    }
}

fn ndcg(gains: &[f64], cutoff: usize) -> Option<f64> {
    if gains.is_empty() {
        return None;
    }
    let dcg = |values: &[f64]| {
        values
            .iter()
            .take(cutoff)
            .enumerate()
            .map(|(index, gain)| gain / ((index + 2) as f64).log2())
            .sum::<f64>()
    };
    let mut ideal = gains.to_vec();
    ideal.sort_by(|left, right| right.total_cmp(left));
    let ideal_dcg = dcg(&ideal);
    (ideal_dcg > 0.0).then(|| dcg(gains) / ideal_dcg)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("feedback evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let (history_path, reviews_path, output_dir, cutoffs) = arguments()?;
    let history_text = fs::read_to_string(&history_path)
        .map_err(|error| format!("cannot read {}: {error}", history_path.display()))?;
    let state_text = fs::read_to_string(&reviews_path)
        .map_err(|error| format!("cannot read {}: {error}", reviews_path.display()))?;
    let state: Value = serde_json::from_str(&state_text)
        .map_err(|error| format!("invalid review state: {error}"))?;
    let reviews = state
        .get("reviews")
        .and_then(Value::as_object)
        .ok_or("review state must contain an object at `reviews`")?;

    let mut counts = Counts::default();
    let mut search_by_id = HashMap::<String, Value>::new();
    for line in history_text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                counts.malformed_history_lines += 1;
                continue;
            }
        };
        if record.get("tool").and_then(Value::as_str) != Some("search") {
            continue;
        }
        if let Some(id) = record.get("trace_id").and_then(Value::as_str) {
            search_by_id.insert(id.to_string(), record);
        }
    }

    let mut ndcg_totals = BTreeMap::<usize, (f64, usize)>::new();
    let mut reciprocal_rank_total = 0.0;
    let mut reciprocal_rank_count = 0;
    let mut dashboard_rows = Vec::<(u64, String, Value)>::new();
    for (search_id, entry) in reviews {
        if entry.get("status").and_then(Value::as_str) != Some("reviewed") {
            continue;
        }
        let Some(feedback) = entry.get("feedback") else {
            continue;
        };
        counts.reviews += 1;
        let record = search_by_id.get(search_id);
        let submitted_at = entry
            .get("submitted_at_ms")
            .and_then(Value::as_u64)
            .or_else(|| {
                record
                    .and_then(|item| item.get("timestamp_ms"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or_default();
        let query = record
            .and_then(|item| item.pointer("/arguments/query"))
            .and_then(Value::as_str)
            .unwrap_or("Search query unavailable");
        let judgments = feedback
            .get("document_judgments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let row = dashboard_review_row(
            search_id,
            submitted_at,
            record.and_then(|item| item.get("provider")),
            query,
            entry
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("reviewed"),
            feedback.get("label"),
            feedback.get("feedback"),
            &judgments,
        );
        dashboard_rows.push((submitted_at, search_id.clone(), row));
        if let Some(label) = feedback.get("label").and_then(Value::as_str) {
            *counts.overall_labels.entry(label.to_string()).or_default() += 1;
        }
        let Some(record) = record else {
            counts.excluded_reviews += 1;
            continue;
        };
        counts.joined_searches += 1;
        let Some(result) = mcp_result(record) else {
            counts.excluded_reviews += 1;
            continue;
        };
        let Some(hits) = result.get("results").and_then(Value::as_array) else {
            counts.excluded_reviews += 1;
            continue;
        };
        let Some(judgments) = feedback.get("document_judgments").and_then(Value::as_array) else {
            counts.excluded_reviews += 1;
            continue;
        };
        let mut labels = HashMap::<String, (usize, f64)>::new();
        let mut valid = judgments.len() == hits.len();
        for judgment in judgments {
            let Some(doc_id) = judgment.get("doc_id").and_then(Value::as_str) else {
                valid = false;
                break;
            };
            let Some(rank) = judgment
                .get("rank")
                .and_then(Value::as_u64)
                .map(|number| number as usize)
            else {
                valid = false;
                break;
            };
            let Some(relevance) = judgment.get("relevance").and_then(Value::as_str) else {
                valid = false;
                break;
            };
            let Some(gain) = relevance_gain(relevance) else {
                valid = false;
                break;
            };
            if labels.insert(doc_id.to_string(), (rank, gain)).is_some() {
                valid = false;
                break;
            }
            *counts
                .relevance_labels
                .entry(relevance.to_string())
                .or_default() += 1;
        }
        if !valid {
            counts.excluded_reviews += 1;
            continue;
        }
        let mut ranked = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(doc_id) = hit
                .get("doc_id")
                .or_else(|| hit.get("id"))
                .and_then(Value::as_str)
            else {
                valid = false;
                break;
            };
            let Some((rank, gain)) = labels.get(doc_id) else {
                valid = false;
                break;
            };
            ranked.push((*rank, *gain));
        }
        if !valid || labels.len() != hits.len() {
            counts.excluded_reviews += 1;
            continue;
        }
        ranked.sort_by_key(|(rank, _)| *rank);
        if ranked
            .iter()
            .enumerate()
            .any(|(index, (rank, _))| *rank != index + 1)
        {
            counts.excluded_reviews += 1;
            continue;
        }
        let gains = ranked.iter().map(|(_, gain)| *gain).collect::<Vec<_>>();
        for cutoff in &cutoffs {
            if let Some(value) = ndcg(&gains, *cutoff) {
                let total = ndcg_totals.entry(*cutoff).or_default();
                total.0 += value;
                total.1 += 1;
            }
        }
        if let Some(rank) = ranked
            .iter()
            .find(|(_, gain)| *gain == 3.0)
            .map(|(rank, _)| *rank)
        {
            reciprocal_rank_total += 1.0 / rank as f64;
        }
        reciprocal_rank_count += 1;
        counts.usable_searches += 1;
        counts.judged_documents += judgments.len();
    }

    dashboard_rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let metric = |cutoff: usize| {
        let (sum, n) = ndcg_totals.get(&cutoff).copied().unwrap_or_default();
        serde_json::json!({ "value": if n == 0 { Value::Null } else { serde_json::json!(sum / n as f64) }, "n": n })
    };
    let evaluation = serde_json::json!({
        "reviewed_searches": counts.reviews,
        "complete_judged_searches": counts.usable_searches,
        "judged_documents": counts.judged_documents,
        "overall_labels": counts.overall_labels,
        "relevance_labels": counts.relevance_labels,
        "ndcg_at_5": metric(5),
        "ndcg_at_10": metric(10),
        "mrr": { "value": if reciprocal_rank_count == 0 { Value::Null } else { serde_json::json!(reciprocal_rank_total / reciprocal_rank_count as f64) }, "n": reciprocal_rank_count },
        "sample_status": if counts.usable_searches < 30 { "small" } else { "usable" },
        "excluded_reviews": counts.excluded_reviews,
        "malformed_history_lines": counts.malformed_history_lines,
    });
    let generated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
        .as_millis() as u64;
    fs::create_dir_all(&output_dir)
        .map_err(|error| format!("cannot create {}: {error}", output_dir.display()))?;
    let mut rows = Vec::new();
    serde_json::to_writer(
        &mut rows,
        &serde_json::json!({
            "_snapshot": {
                "schema_version": 1,
                "generated_at": generated_at_ms,
                "reviewed_searches": dashboard_rows.len(),
                "evaluation": evaluation,
            }
        }),
    )
    .map_err(|error| error.to_string())?;
    rows.push(b'\n');
    for (_, _, row) in &dashboard_rows {
        let row_start = rows.len();
        serde_json::to_writer(&mut rows, row).map_err(|error| error.to_string())?;
        rows.push(b'\n');
        if rows.len() - row_start > MAX_DASHBOARD_ROW_BYTES {
            return Err("dashboard review row exceeds the 512 KiB limit".to_string());
        }
    }
    // Header and rows are one snapshot: readers cannot pair rows from one run
    // with summary metrics from another if publication is interrupted.
    write_atomic(&output_dir.join("search-feedback-dashboard.jsonl"), &rows)?;

    println!("Offline search-feedback evaluation");
    println!("History: {}", history_path.display());
    println!("Reviews: {}", reviews_path.display());
    println!("Dashboard snapshots: {}", output_dir.display());
    println!("Reviewed searches: {}", counts.reviews);
    println!("Joined to search records: {}", counts.joined_searches);
    println!(
        "Complete usable judgments: {} searches, {} query-document labels",
        counts.usable_searches, counts.judged_documents
    );
    println!("Excluded or unmatched reviews: {}", counts.excluded_reviews);
    println!(
        "Malformed history lines skipped: {}",
        counts.malformed_history_lines
    );
    println!("Overall labels: {}", format_counts(&counts.overall_labels));
    println!(
        "Document relevance labels: {}",
        format_counts(&counts.relevance_labels)
    );
    for cutoff in &cutoffs {
        match ndcg_totals.get(cutoff) {
            Some((sum, count)) if *count > 0 => println!(
                "Baseline macro NDCG@{cutoff}: {:.4} ({count} searches)",
                sum / *count as f64
            ),
            _ => println!("Baseline macro NDCG@{cutoff}: unavailable"),
        }
    }
    if reciprocal_rank_count > 0 {
        println!(
            "Baseline MRR (relevant only): {:.4} ({reciprocal_rank_count} searches)",
            reciprocal_rank_total / reciprocal_rank_count as f64
        );
    } else {
        println!("Baseline MRR (relevant only): unavailable");
    }
    println!("Interpretation: metrics describe only returned documents with complete feedback; they are not corpus-wide recall and do not measure a trained model.");
    Ok(())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<(), String> {
    let mut temp = path.to_path_buf();
    temp.set_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(|error| format!("cannot create {}: {error}", temp.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", temp.display()))?;
    fs::rename(&temp, path).map_err(|error| format!("cannot publish {}: {error}", path.display()))
}

fn format_counts(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .map(|(label, count)| format!("{label}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_dashboard_review_fields_fit_the_pagination_row_limit() {
        let judgments = (0..MAX_DASHBOARD_DOCUMENT_JUDGMENTS)
            .map(|rank| {
                serde_json::json!({
                    "doc_id": "\u{0000}".repeat(2_000),
                    "rank": rank + 1,
                    "source_hash": "\u{0000}".repeat(1_000),
                    "relevance": "\u{0000}".repeat(500),
                    "feedback": "\u{0000}".repeat(2_000),
                    "unexpected_unbounded_field": "\u{0000}".repeat(100_000),
                })
            })
            .collect::<Vec<_>>();
        let row = dashboard_review_row(
            &"\u{0000}".repeat(2_000),
            123,
            Some(&Value::String("\u{0000}".repeat(1_000))),
            &"\u{0000}".repeat(10_000),
            "reviewed",
            Some(&Value::String("helpful".to_string())),
            Some(&Value::String("\u{0000}".repeat(20_000))),
            &judgments,
        );
        let bytes = serde_json::to_vec(&row).unwrap();
        assert!(bytes.len() < MAX_DASHBOARD_ROW_BYTES);
        assert_eq!(row["document_judgments"].as_array().unwrap().len(), 50);
        assert!(row["document_judgments"][0]
            .get("unexpected_unbounded_field")
            .is_none());
    }
}
