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

mod lexical_reuse {
    use super::super::model::{DocRecord, MemoryIndex};
    use super::lexical_test_doc;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let suffix = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "membrick-lexical-reuse-{name}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn dir(&self) -> PathBuf {
            self.0.join("lexical")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn build(records: Vec<DocRecord>, dir: &Path) -> MemoryIndex {
        MemoryIndex::from_records_with_lexical_dir(records, Some(dir), false, false, false)
    }

    fn docs() -> Vec<DocRecord> {
        vec![
            lexical_test_doc("doc-a", "pineapple harvest schedule"),
            lexical_test_doc("doc-b", "banana shipping notes"),
        ]
    }

    /// The commit's operation stamp and segment files: unchanged exactly when
    /// the index was not written.
    fn state(dir: &Path) -> (String, BTreeMap<String, SystemTime>) {
        let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap();
        let files = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.metadata().unwrap().modified().unwrap(),
                )
            })
            .collect();
        (meta, files)
    }

    fn hits(index: &MemoryIndex, query: &str) -> Vec<(String, u32)> {
        let mut hits = index
            .lexical_bm25(query, 10, None)
            .unwrap()
            .into_iter()
            .map(|(id, score)| (id, score.to_bits()))
            .collect::<Vec<_>>();
        hits.sort();
        hits
    }

    #[test]
    fn an_unchanged_index_is_reused_without_a_write_and_scores_the_same() {
        let root = TempRoot::new("unchanged");
        let first = build(docs(), &root.dir());
        let before = state(&root.dir());
        let mut reordered = docs();
        reordered.reverse();
        let reopened = build(reordered, &root.dir());
        assert_eq!(state(&root.dir()), before, "nothing was written");
        for query in ["pineapple", "banana notes", "schedule"] {
            assert!(!hits(&first, query).is_empty());
            assert_eq!(hits(&first, query), hits(&reopened, query));
        }
    }

    #[test]
    fn any_change_to_the_indexed_texts_rebuilds() {
        type Change = (&'static str, fn(&mut Vec<DocRecord>));
        let changes: [Change; 7] = [
            ("content", |docs| {
                docs[0].section_chunks[0].content = "mango harvest schedule".into()
            }),
            ("heading", |docs| {
                docs[0].section_chunks[0].heading = "Mango".into()
            }),
            ("terms", |docs| {
                docs[0].section_chunks[0]
                    .important_terms
                    .push("mango".into())
            }),
            ("entities", |docs| {
                docs[0].section_chunks[0].key_entities.push("mango".into())
            }),
            ("temporal", |docs| {
                docs[0].temporal_terms.push("mango".into())
            }),
            ("added", |docs| {
                docs.push(lexical_test_doc("doc-c", "mango crates"))
            }),
            ("removed", |docs| {
                docs.remove(1);
            }),
        ];
        for (name, change) in changes {
            let root = TempRoot::new(name);
            build(docs(), &root.dir());
            let before = state(&root.dir());
            let mut changed = docs();
            change(&mut changed);
            let reopened = build(changed.clone(), &root.dir());
            assert_ne!(state(&root.dir()), before, "{name} rebuilds");
            let fresh =
                MemoryIndex::from_records_with_lexical_dir(changed, None, false, false, false);
            for query in ["mango", "banana", "pineapple"] {
                assert_eq!(hits(&reopened, query), hits(&fresh, query), "{name}");
            }
        }
    }

    #[test]
    fn a_changed_semantic_tag_rebuilds_although_the_content_hash_is_the_same() {
        let root = TempRoot::new("tag");
        let plain = docs();
        build(plain.clone(), &root.dir());
        let before = state(&root.dir());
        let mut tagged = plain;
        tagged[0].content_hash = "same".into();
        build(tagged.clone(), &root.dir());
        assert_eq!(
            state(&root.dir()),
            before,
            "the content hash is not indexed"
        );
        tagged[0].key_phrases.push(crate::source::KeyPhrase {
            text: "mango".into(),
            kind: "semantic_tag".into(),
            origin: None,
        });
        let reopened = build(tagged, &root.dir());
        assert_ne!(state(&root.dir()), before);
        assert_eq!(hits(&reopened, "mango").len(), 1);
    }

    #[test]
    fn an_index_without_a_fingerprint_is_rebuilt_and_then_reused() {
        let root = TempRoot::new("unstamped");
        build(docs(), &root.dir());
        // A commit by an older version, which sets no payload, of other documents.
        {
            let index = tantivy::Index::open_in_dir(root.dir()).unwrap();
            let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
            writer.delete_all_documents().unwrap();
            // As many documents as the records, so only the missing
            // fingerprint tells them apart.
            let doc_id = index.schema().get_field("doc_id").unwrap();
            for id in ["old-a", "old-b"] {
                writer.add_document(tantivy::doc!(doc_id => id)).unwrap();
            }
            writer.commit().unwrap();
        }
        let rebuilt = build(docs(), &root.dir());
        assert_eq!(hits(&rebuilt, "pineapple").len(), 1);
        let before = state(&root.dir());
        build(docs(), &root.dir());
        assert_eq!(state(&root.dir()), before);
    }

    #[test]
    fn a_damaged_or_missing_file_is_rebuilt() {
        for damage in ["flip", "truncate", "remove"] {
            let root = TempRoot::new(damage);
            build(docs(), &root.dir());
            let file = std::fs::read_dir(root.dir())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
                .unwrap();
            match damage {
                "flip" => {
                    let mut bytes = std::fs::read(&file).unwrap();
                    bytes[0] ^= 0xff;
                    std::fs::write(&file, bytes).unwrap();
                }
                "truncate" => {
                    let bytes = std::fs::read(&file).unwrap();
                    std::fs::write(&file, &bytes[..bytes.len() / 2]).unwrap();
                }
                _ => std::fs::remove_file(&file).unwrap(),
            }
            let damaged = std::fs::read_to_string(root.dir().join("meta.json")).unwrap();
            let reopened = build(docs(), &root.dir());
            let repaired = std::fs::read_to_string(root.dir().join("meta.json")).unwrap();
            assert_ne!(repaired, damaged, "{damage}: rebuilt");
            assert_eq!(hits(&reopened, "pineapple").len(), 1, "{damage}");
            assert_eq!(hits(&reopened, "banana").len(), 1, "{damage}");
            let before = state(&root.dir());
            build(docs(), &root.dir());
            assert_eq!(state(&root.dir()), before, "{damage}: whole again");
        }
    }
}
