use super::helpers::*;
use super::model::*;

use crate::query_expansion::normalize_for_index;

#[test]
fn text_overlap_prefers_matching_candidate() {
    let query = tokenize_query_terms(&normalize_for_index("What degree did I graduate with?"));
    let good = tokenize_query_terms(&normalize_for_index(
        "I graduated with a computer science degree.",
    ));
    let bad = tokenize_query_terms(&normalize_for_index("I cooked pasta and watched a movie."));

    assert!(token_overlap_ratio(&query, &good) > token_overlap_ratio(&query, &bad));
    assert!(ngram_overlap_ratio(&query, &good, 2) > ngram_overlap_ratio(&query, &bad, 2));
    assert!(lcs_ratio(&query, &good) > lcs_ratio(&query, &bad));
}

#[test]
fn persistent_memory_index_build_waits_for_shared_tantivy_guard() {
    use crate::index::write_lock::PersistentIndexWriteLock;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "membrick-index-build-lock-{}-{suffix}",
        std::process::id()
    ));
    let lexical_dir = root.join("lexical");
    let held = PersistentIndexWriteLock::acquire(&lexical_dir).unwrap();

    let (send, receive) = mpsc::channel();
    let builder_dir = lexical_dir.clone();
    let builder = std::thread::spawn(move || {
        let index = MemoryIndex::from_records_with_lexical_dir(
            Vec::new(),
            Some(&builder_dir),
            false,
            false,
            false,
        );
        send.send(index.lexical.is_some()).unwrap();
    });

    assert!(receive.recv_timeout(Duration::from_millis(150)).is_err());
    drop(held);
    assert!(receive
        .recv_timeout(Duration::from_secs(2))
        .expect("index build should proceed after the shared guard is released"));
    builder.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

fn lexical_test_doc(doc_id: &str, content: &str) -> DocRecord {
    DocRecord {
        doc_id: doc_id.to_string(),
        source: format!("source://{doc_id}"),
        content: content.into(),
        timestamp: None,
        doc_length: content.split_whitespace().count(),
        author_agent: None,
        group_id: None,
        filters: std::collections::BTreeMap::new(),
        probable_topic: None,
        doc_type_guess: None,
        headings: Vec::new(),
        doc_links: Vec::new(),
        temporal_terms: Vec::new(),
        key_entities: Vec::new(),
        important_terms: Vec::new(),
        key_phrases: Vec::new(),
        key_phrase_extraction_hash: String::new(),
        section_chunks: vec![SectionChunk {
            chunk_id: format!("{doc_id}::0"),
            heading: String::new(),
            content: content.into(),
            start_line: 1,
            end_line: 1,
            timestamp: None,
            key_entities: Vec::new(),
            important_terms: Vec::new(),
        }],
        embedding: None,
        top_claims: Vec::new(),
        provenance: Provenance {
            source: format!("source://{doc_id}"),
            timestamp: None,
            ner_provider: "heuristic".to_string(),
            term_ranker: "yake".to_string(),
            index_version: "v1".to_string(),
        },
        content_hash: String::new(),
        semantic_tags: Vec::new(),
    }
}

#[test]
fn reopened_lexical_folder_serves_stored_contents_with_a_manual_reader() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "membrick-lexical-rebuild-{}-{suffix}",
        std::process::id()
    ));
    let lexical_dir = root.join("lexical");

    let first = MemoryIndex::from_records_with_lexical_dir(
        vec![lexical_test_doc("doc-a", "pineapple harvest schedule")],
        Some(&lexical_dir),
        false,
        false,
        false,
    );
    let first_hits = first.lexical_bm25("pineapple", 5, None).unwrap();
    assert_eq!(first_hits.len(), 1);
    assert!(first_hits.contains_key("doc-a"));

    // A built folder is reused as is and never written again, so a reopened
    // index serves the stored contents and a held reader has nothing to follow.
    let reopened = MemoryIndex::from_records_with_lexical_dir(
        vec![lexical_test_doc("doc-a", "pineapple harvest schedule")],
        Some(&lexical_dir),
        false,
        false,
        false,
    );
    let reopened_hits = reopened.lexical_bm25("pineapple", 5, None).unwrap();
    assert_eq!(reopened_hits.len(), 1);
    assert!(reopened_hits.contains_key("doc-a"));

    // Different contents live in a different folder and answer on their own.
    let other = MemoryIndex::from_records_with_lexical_dir(
        vec![
            lexical_test_doc("doc-a", "pineapple harvest schedule"),
            lexical_test_doc("doc-b", "pineapple canning notes"),
        ],
        Some(&root.join("lexical-other")),
        false,
        false,
        false,
    );
    assert_eq!(other.lexical_bm25("pineapple", 5, None).unwrap().len(), 2);
    assert_eq!(first.lexical_bm25("pineapple", 5, None).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn timestamped_chunk_detects_chunk_level_temporal_anchor() {
    let doc = DocRecord {
        doc_id: "doc-1".to_string(),
        source: "source://doc-1".to_string(),
        content: "content".into(),
        timestamp: None,
        doc_length: 7,
        author_agent: None,
        group_id: None,
        filters: std::collections::BTreeMap::new(),
        probable_topic: None,
        doc_type_guess: None,
        headings: vec!["Overview".to_string()],
        doc_links: vec![],
        temporal_terms: vec![],
        key_entities: vec![],
        important_terms: vec![],
        key_phrases: vec![],
        key_phrase_extraction_hash: String::new(),
        section_chunks: vec![SectionChunk {
            chunk_id: "doc-1::0".to_string(),
            heading: "Overview".to_string(),
            content: "content".into(),
            start_line: 1,
            end_line: 1,
            timestamp: Some("2024-05-10".to_string()),
            key_entities: vec![],
            important_terms: vec![],
        }],
        embedding: None,
        top_claims: vec![],
        provenance: Provenance {
            source: "source://doc-1".to_string(),
            timestamp: None,
            ner_provider: "heuristic".to_string(),
            term_ranker: "yake".to_string(),
            index_version: "v1".to_string(),
        },
        content_hash: String::new(),
        semantic_tags: Vec::new(),
    };

    assert!(doc_has_timestamped_chunk(&doc));
}

#[test]
fn normalize_temporal_bounds_orders_reversed_ranges_and_preserves_same_day() {
    let (start, end) = normalize_temporal_bounds(Some("2024-05-10"), Some("2024-05-01"));
    assert_eq!(
        start,
        Some(chrono::NaiveDate::from_ymd_opt(2024, 5, 1).expect("valid date"))
    );
    assert_eq!(
        end,
        Some(chrono::NaiveDate::from_ymd_opt(2024, 5, 10).expect("valid date"))
    );

    let (same_start, same_end) = normalize_temporal_bounds(Some("2024-05-10"), Some("2024-05-10"));
    assert_eq!(same_start, same_end);
    assert_eq!(
        same_start,
        Some(chrono::NaiveDate::from_ymd_opt(2024, 5, 10).expect("valid date"))
    );
}
