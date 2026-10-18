use super::enrich::*;
use super::key_phrases::*;
use super::review::*;
use super::search::*;
use super::*;
use crate::PipelineOptions;

impl MemoryService {
    fn review_search_ready(
        &self,
        root: &Path,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let response = self.review_search_mcp(root, arguments)?;
            if response["status"] != "indexing" {
                return Ok(response);
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "search history index did not become ready: {response}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[test]
fn search_feedback_evaluation_uses_complete_ranked_judgments_only() {
    let record = crate::telemetry::McpCallRecord {
        trace_id: Some("search-1".into()),
        timestamp_ms: 1,
        provider: "codex".into(),
        tool: "search".into(),
        session_id: None,
        session_attribution: "absent".into(),
        request_id: None,
        arguments: serde_json::json!({"query":"example"}),
        duration_ms: 1,
        error: false,
        response: serde_json::json!({"result":{"content":[{"text":serde_json::json!({
                "results":[{"doc_id":"doc:a"},{"doc_id":"doc:b"}]
            }).to_string()}]}}),
    };
    let records = HashMap::from([("search-1".into(), record)]);
    let reviews = serde_json::json!({
        "search-1":{"feedback":{"label":"partially_helpful","document_judgments":[
            {"doc_id":"doc:a","rank":1,"relevance":"not_relevant"},
            {"doc_id":"doc:b","rank":2,"relevance":"relevant"}
        ]}},
        "search-2":{"feedback":{"label":"not_helpful","document_judgments":[
            {"doc_id":"doc:missing","rank":1,"relevance":"relevant"}
        ]}}
    });
    let evaluation = search_feedback_evaluation(reviews.as_object().unwrap(), &records);

    assert_eq!(evaluation["reviewed_searches"], 2);
    assert_eq!(evaluation["complete_judged_searches"], 1);
    assert_eq!(evaluation["judged_documents"], 3);
    assert_eq!(evaluation["mrr"]["n"], 1);
    assert_eq!(evaluation["mrr"]["value"], 0.5);
    assert!((evaluation["ndcg_at_5"]["value"].as_f64().unwrap() - 0.6309297536).abs() < 1e-8);
    assert_eq!(evaluation["overall_labels"]["not_helpful"], 1);
    assert_eq!(evaluation["sample_status"], "small");
}

#[test]
fn persistent_receipt_cache_is_bounded_and_evicted_retries_are_exact() {
    let dir = canonical_temp_dir("bounded-receipts");
    let mut owner = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
    let request = AddRequest {
        request_id: "original".into(),
        user_id: "alice".into(),
        session_id: "session".into(),
        messages: vec![Message {
            role: "user".into(),
            content: "remember violet lantern".into(),
            timestamp: None,
            expires_at_ms: None,
            supersedes_id: None,
        }],
    };
    let response = owner.add(request.clone()).unwrap();
    let mut expected = response.adjudication.unwrap();
    expected
        .retired_doc_ids
        .push("persisted-receipt-marker".into());
    let mut entries = Vec::new();
    for i in 0..532 {
        let mut receipt = expected.clone();
        receipt.request_id = format!("other-{i}");
        entries.push((("alice".to_string(), receipt.request_id.clone()), receipt));
    }
    entries.push((
        ("alice".to_string(), "original".to_string()),
        expected.clone(),
    ));
    std::fs::write(
        dir.join("receipts.json"),
        serde_json::to_vec(&entries).unwrap(),
    )
    .unwrap();
    // A newer journal receipt must win over the snapshot.
    expected.retired_doc_ids.push("newer-journal-marker".into());
    let journal = serde_json::to_vec(&vec![(
        ("alice".to_string(), "original".to_string()),
        expected.clone(),
    )])
    .unwrap();
    std::fs::write(dir.join("receipts.jsonl"), journal).unwrap();
    drop(owner);
    let mut owner = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
    assert!(
        owner.request_receipts.len() <= 512,
        "loaded {} receipts into memory",
        owner.request_receipts.len()
    );
    owner
        .request_receipts
        .remove(&("alice".into(), "original".into()));
    let retry = owner.add(request).unwrap();
    assert_eq!(
        serde_json::to_value(retry.adjudication.unwrap()).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

fn service() -> MemoryService {
    MemoryService::in_memory(PipelineOptions::default())
}

#[test]
fn relative_freshness_runs_once_after_memoryservice_result_merge() {
    let policy = crate::temporal::FreshnessPolicy {
        mode: crate::temporal::FreshnessMode::Relative,
        ..crate::temporal::FreshnessPolicy::default()
    };
    let mut service = MemoryService::in_memory(PipelineOptions {
        freshness_policy: policy,
        structured_fact_retrieval: false,
        conversational_rerank: false,
        ..PipelineOptions::default()
    });
    for (doc_id, timestamp) in [
        ("freshness-old", "2024-05-02T12:00:00Z"),
        ("freshness-latest", "2024-07-01T12:00:00Z"),
    ] {
        let mut document = direct_doc(doc_id, "Aurora release policy uses staged rollout.");
        document.timestamp = Some(timestamp.to_string());
        document
            .filters
            .insert(USER_FILTER.into(), "freshness-test-user".into());
        service.upsert(document);
    }
    service.refresh_index().unwrap();

    let hits = service
        .search_with_filters(
            "latest Aurora release policy",
            "user",
            None,
            2,
            &BTreeMap::from([(USER_FILTER.into(), "freshness-test-user".into())]),
            Some("2024-07-01"),
        )
        .unwrap();
    assert_eq!(
        hits.first().map(|hit| hit.doc_id.as_str()),
        Some("freshness-latest"),
        "MemoryService should apply relative freshness after candidate retrieval"
    );
    assert!(hits[0].score_breakdown.recency_score > hits[1].score_breakdown.recency_score);
}

#[test]
fn relative_freshness_sort_preserves_order_for_equal_scores() {
    let result = |doc_id: &str, score: f32, structured: bool| crate::SearchResult {
        doc_id: doc_id.into(),
        source: String::new(),
        group_id: None,
        score,
        score_breakdown: crate::index::ScoreBreakdown::default(),
        matched_entities: Vec::new(),
        matched_terms: Vec::new(),
        probable_topic: None,
        doc_type_guess: None,
        semantic_status: None,
        superseded_by: None,
        relation_confidence: None,
        relation_evidence: structured
            .then(|| "claim".to_string())
            .into_iter()
            .collect(),
        comparison_operands: Vec::new(),
    };

    let sorted = sort_by_adjusted_score_with_structured_priority(vec![
        result("z-first", 3.0, false),
        result("a-second", 3.0, false),
        result("structured", 1.0, true),
    ]);

    assert_eq!(
        sorted
            .iter()
            .map(|hit| hit.doc_id.as_str())
            .collect::<Vec<_>>(),
        ["structured", "z-first", "a-second"],
        "equal-score results must retain the retrieval order within each tier"
    );
}

#[test]
fn ordinary_memories_do_not_need_a_visibility_allow_list() {
    let mut service = service();
    assert!(!service.requires_visibility_allow_list());
    service
        .add(AddRequest {
            request_id: "expiring-memory".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "temporary deployment decision".into(),
                expires_at_ms: Some(u64::MAX),
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();
    assert!(service.requires_visibility_allow_list());
}

#[test]
fn session_observations_are_not_indexed_by_memoryservice() {
    let mut service = service();
    let mut observation = direct_doc("observation-1", "rare raw transcript phrase");
    observation.filters.insert(
        "document_type".into(),
        SESSION_OBSERVATION_DOCUMENT_TYPE.into(),
    );
    let turn = direct_doc("turn-1", "searchable turn result");
    service.upsert(observation);
    service.upsert(turn);
    service.refresh_index().unwrap();

    assert!(
        service
            .store
            .source_document_by_id("observation-1")
            .is_none(),
        "raw session observation entered the searchable store"
    );

    let hits = service
        .search_with_filters(
            "rare raw transcript phrase",
            "user",
            None,
            10,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(
        hits.is_empty(),
        "raw observation leaked into search results"
    );

    let hits = service
        .search_with_filters(
            "searchable turn result",
            "user",
            None,
            10,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.doc_id.as_str())
            .collect::<Vec<_>>(),
        ["turn-1"]
    );
}

#[test]
fn opening_persistent_memory_removes_legacy_session_observations() {
    let dir = canonical_temp_dir("prune-session-observations");
    let mut store = IndexStore::at_path(&dir, PipelineOptions::default()).unwrap();
    let mut observation = direct_doc("legacy-observation", "duplicated raw message");
    observation.filters.insert(
        "document_type".into(),
        SESSION_OBSERVATION_DOCUMENT_TYPE.into(),
    );
    store.upsert(observation);
    store.upsert(direct_doc("retained-turn", "searchable turn result"));
    store.refresh().unwrap();
    drop(store);

    let service = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
    assert!(service
        .store
        .source_document_by_id("legacy-observation")
        .is_none());
    assert!(service
        .store
        .source_document_by_id("retained-turn")
        .is_some());
    drop(service);

    let reopened = IndexStore::at_path(&dir, PipelineOptions::default()).unwrap();
    assert!(reopened
        .source_document_by_id("legacy-observation")
        .is_none());
    assert!(reopened.source_document_by_id("retained-turn").is_some());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "roo-runtime"
))]
#[test]
fn hook_session_pointer_is_written_through_memory_service() {
    let root = canonical_temp_dir("hook-session-service");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    MemoryService::note_active_session_at_path(&root, "claude", "session-service-test");

    let service = MemoryService::at_path(&root, PipelineOptions::default()).unwrap();
    assert_eq!(
        service.current_session_id("claude").as_deref(),
        Some("session-service-test")
    );
    MemoryService::note_active_session_at_path(&root, "claude", "parallel-session");
    assert_eq!(service.current_session_id("claude"), None);
    MemoryService::deactivate_session_at_path(&root, "claude", "session-service-test");
    assert_eq!(
        service.current_session_id("claude").as_deref(),
        Some("parallel-session")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn add_preserves_memories_with_colliding_legacy_identifiers() {
    for ((owner_a, request_a), (owner_b, request_b)) in [
        (("alice", "team:r1"), ("alice:team", "r1")),
        (("Alice", "r1"), ("alice", "r1")),
        (("alice  team", "r1"), ("alice team", "r1")),
        (("alice", "R1"), ("alice", "r1")),
    ] {
        let mut service = service();
        add_message(
            &mut service,
            request_a,
            owner_a,
            "session",
            "original memory",
        );
        add_message(&mut service, request_b, owner_b, "session", "second memory");
        let docs = service.store.source_documents();
        assert_eq!(
            docs.len(),
            2,
            "both memories must survive: {owner_a}/{request_a} vs {owner_b}/{request_b}"
        );
        assert!(docs
            .iter()
            .any(|doc| owns_memory(doc, owner_a) && doc.content == "user: original memory"));
        assert!(docs
            .iter()
            .any(|doc| owns_memory(doc, owner_b) && doc.content == "user: second memory"));
    }
}

#[test]
fn add_rejects_existing_document_owned_by_another_user() {
    let mut service = service();
    let target_id = doc_id_for("alice", "r1");
    let mut existing = direct_doc(&target_id, "original memory");
    existing.filters.insert(USER_FILTER.into(), "bob".into());
    service.upsert(existing);
    let error = service
        .add(AddRequest {
            user_id: "alice".into(),
            request_id: "r1".into(),
            session_id: "session".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "replacement".into(),
                timestamp: None,
                expires_at_ms: None,
                supersedes_id: None,
            }],
        })
        .expect_err("must refuse a cross-owner replacement");
    assert!(error.to_string().contains("conflict"));
    let preserved = service.store.source_document_by_id(&target_id).unwrap();
    assert!(owns_memory(preserved, "bob"));
    assert_eq!(preserved.content, "original memory");
}

#[test]
fn add_identity_conflict_does_not_write_earlier_messages_or_supersessions() {
    let mut service = service();
    let target_id = memory_document_id("alice", "r1", 1);
    let mut existing = direct_doc(&target_id, "original memory");
    existing.filters.insert(USER_FILTER.into(), "alice".into());
    existing
        .filters
        .insert("request_id".into(), "different-request".into());
    service.upsert(existing);
    let result = service.add(AddRequest {
        user_id: "alice".into(),
        request_id: "r1".into(),
        session_id: "session".into(),
        messages: vec![
            Message {
                role: "user".into(),
                content: "first".into(),
                timestamp: None,
                expires_at_ms: None,
                supersedes_id: Some("old-memory".into()),
            },
            Message {
                role: "user".into(),
                content: "second".into(),
                timestamp: None,
                expires_at_ms: None,
                supersedes_id: None,
            },
        ],
    });
    assert!(result.is_err());
    assert_eq!(service.store.source_documents().len(), 1);
    assert_eq!(
        service
            .store
            .source_document_by_id(&target_id)
            .unwrap()
            .content,
        "original memory"
    );
    assert!(!service
        .superseded_ids
        .contains(&("alice".into(), "old-memory".into())));
    assert!(!service
        .request_fingerprints
        .contains_key(&("alice".into(), "r1".into())));
}

#[test]
fn legacy_memory_retry_keeps_stored_id_after_reopening() {
    let dir = canonical_temp_dir("legacy-memory-id-retry");
    let request = AddRequest {
        user_id: "alice".into(),
        request_id: "team:r1".into(),
        session_id: "session".into(),
        messages: vec![Message {
            role: "user".into(),
            content: "original memory".into(),
            timestamp: None,
            expires_at_ms: None,
            supersedes_id: None,
        }],
    };
    let legacy_id = crate::ids::stable_doc_id_from_source("alice:team:r1:0");
    {
        let mut service = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
        let mut legacy = direct_doc(&legacy_id, "user: original memory");
        legacy.group_id = Some(request.session_id.clone());
        legacy
            .filters
            .insert(USER_FILTER.into(), request.user_id.clone());
        legacy
            .filters
            .insert("request_id".into(), request.request_id.clone());
        legacy.filters.insert(
            "request_fingerprint".into(),
            request_fingerprint(&request.session_id, &request.messages).unwrap(),
        );
        service.upsert(legacy);
        service.refresh().unwrap();
    }
    {
        let mut service = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
        service.add(request.clone()).unwrap();
        assert_eq!(service.store.source_documents().len(), 1);
        assert_eq!(
            service
                .store
                .source_document_by_id(&legacy_id)
                .unwrap()
                .content,
            "user: original memory"
        );
        add_message(&mut service, "r1", "alice:team", "session", "second memory");
        assert_eq!(service.store.source_documents().len(), 2);
        service.add(request).unwrap();
        assert_eq!(service.store.source_documents().len(), 2);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn persistent_api_mutations_write_content_free_audit_entries() {
    let dir = canonical_temp_dir("memory-mutation-audit");
    let mut service = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
    let request_id = "audit-request";
    let content = "private decision text";
    service
        .add(AddRequest {
            request_id: request_id.into(),
            user_id: "alice".into(),
            session_id: "audit-session".into(),
            messages: vec![Message {
                role: "assistant".into(),
                timestamp: None,
                content: content.into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
        })
        .unwrap();
    let memory_id = memory_document_id("alice", request_id, 0);
    service
        .update(UpdateRequest {
            user_id: "alice".into(),
            memory_id: memory_id.clone(),
            content: "updated private decision text".into(),
            role: None,
            timestamp: None,
            expires_at_ms: None,
        })
        .unwrap();
    assert!(service.delete("alice", &memory_id).unwrap());
    service
        .record_capture_mutation("claude", "mcp", "capture-session", &["capture-doc".into()])
        .unwrap();

    let journal = std::fs::read_to_string(dir.join("mutations.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0]["action"], "add");
    assert_eq!(records[0]["request_id"], request_id);
    assert_eq!(records[1]["action"], "update");
    assert_eq!(records[1]["memory_ids"][0], memory_id);
    assert_eq!(records[2]["action"], "delete");
    assert!(records[2]["memory_ids"][0] == memory_id);
    assert_eq!(records[3]["action"], "capture:claude");
    assert_eq!(records[3]["request_id"], "capture-session");
    assert_eq!(records[3]["memory_ids"][0], "capture-doc");
    assert!(!journal.contains(content));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Temp dir with symlinks resolved (on macOS `TMPDIR` lives under
/// `/var`, which is a symlink to `/private/var`; comparing or
/// reopening paths through the unresolved prefix breaks).
fn canonical_temp_dir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(format!("membrick-{name}-{}", std::process::id()))
}

fn bare_search_result(doc_id: &str, score: f32) -> crate::SearchResult {
    crate::SearchResult {
        doc_id: doc_id.to_string(),
        source: "test".to_string(),
        group_id: None,
        score,
        score_breakdown: Default::default(),
        matched_entities: vec![],
        matched_terms: vec![],
        probable_topic: None,
        doc_type_guess: None,
        semantic_status: None,
        superseded_by: None,
        relation_confidence: None,
        relation_evidence: vec![],
        comparison_operands: vec![],
    }
}

#[test]
fn comparison_merge_interleaves_sides_and_combines_shared_hits() {
    let mut left = bare_search_result("shared", 0.8);
    left.comparison_operands = vec!["old interface".into()];
    let mut left_only = bare_search_result("old-only", 0.7);
    left_only.comparison_operands = vec!["old interface".into()];
    let mut right = bare_search_result("shared", 0.9);
    right.comparison_operands = vec!["new interface".into()];
    let mut right_only = bare_search_result("new-only", 0.6);
    right_only.comparison_operands = vec!["new interface".into()];

    let merged =
        interleave_comparison_results(vec![vec![left, left_only], vec![right, right_only]], 3);

    assert_eq!(
        merged
            .iter()
            .map(|result| result.doc_id.as_str())
            .collect::<Vec<_>>(),
        vec!["shared", "old-only", "new-only"]
    );
    assert_eq!(
        merged[0].comparison_operands,
        vec!["old interface", "new interface"]
    );
    assert_eq!(merged[0].score, 0.9);
}

#[test]
fn add_is_immediately_searchable() {
    let mut service = service();
    service
        .add(AddRequest {
            request_id: "request-1".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "I prefer dark mode in every editor".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();
    let response = service
        .search(SearchRequest {
            query: "dark mode editor".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 100,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("dark mode"));
}

#[test]
fn comparison_search_retrieves_both_named_sides_through_memory_service() {
    let mut service = service();
    for (request_id, content) in [
        (
            "old",
            "The old command interface used --claude-code-install.",
        ),
        (
            "new",
            "The new command interface uses membrick install claude.",
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: request_id.into(),
            })
            .unwrap();
    }
    let filters = BTreeMap::from([(USER_FILTER.to_string(), "user-a".to_string())]);

    let results = service
        .search_with_filters_cached(
            "Compare old and new command interface",
            "user-a",
            None,
            10,
            &filters,
            None,
        )
        .unwrap();

    assert!(results.iter().any(|result| {
        result
            .comparison_operands
            .iter()
            .any(|operand| operand.starts_with("old "))
    }));
    assert!(results.iter().any(|result| {
        result
            .comparison_operands
            .iter()
            .any(|operand| operand.starts_with("new "))
    }));

    let response = service
        .search(SearchRequest {
            query: "Compare old and new command interface".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(response.data.iter().any(|result| {
        result
            .comparison_operands
            .iter()
            .any(|operand| operand.starts_with("old "))
    }));
    assert!(response.data.iter().any(|result| {
        result
            .comparison_operands
            .iter()
            .any(|operand| operand.starts_with("new "))
    }));
}

#[test]
fn explanation_query_terms_retrieve_a_rationale_document() {
    let mut service = service();
    for (request_id, content) in [
            (
                "command-history",
                "The command interface used six provider-specific installation options.",
            ),
            (
                "command-rationale",
                "Decision rationale: we changed the command interface because combining provider commands made installation easier. This records why the old options were replaced.",
            ),
        ] {
            service
                .add(AddRequest {
                    request_id: request_id.into(),
                    messages: vec![Message {
                        role: "user".into(),
                        timestamp: None,
                        content: content.into(),
                        expires_at_ms: None,
                        supersedes_id: None,
                    }],
                    user_id: "user-a".into(),
                    session_id: request_id.into(),
                })
                .unwrap();
        }

    let response = service
        .search(SearchRequest {
            query: "Why did we change the command interface?".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();

    assert!(response
        .data
        .iter()
        .any(|result| result.content.contains("Decision rationale")));
}

#[test]
fn search_cannot_cross_user_boundaries() {
    let mut service = service();
    for (request_id, user_id, content) in [
        ("request-a", "user-a", "the secret project uses amber"),
        ("request-b", "user-b", "the secret project uses cobalt"),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: user_id.into(),
                session_id: "session".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "secret project color".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 100,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(!response.data.is_empty(), "the positive control must match");
    assert!(response
        .data
        .iter()
        .all(|memory| memory.content.contains("amber")));
}

#[test]
fn identical_request_ids_are_isolated_between_users() {
    let mut service = service();
    for (user_id, content) in [("user-a", "alpha secret"), ("user-b", "beta secret")] {
        service
            .add(AddRequest {
                request_id: "same-request".into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: user_id.into(),
                session_id: "session".into(),
            })
            .unwrap();
    }
    let a = service
        .search(SearchRequest {
            query: "secret".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    let b = service
        .search(SearchRequest {
            query: "secret".into(),
            options: None,
            user_id: "user-b".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(a.data.len(), 1);
    assert_eq!(b.data.len(), 1);
    assert!(a.data[0].content.contains("alpha"));
    assert!(b.data[0].content.contains("beta"));
}

#[test]
fn request_fingerprint_is_a_digest_and_includes_session_id() {
    let mut service = service();
    let messages = vec![Message {
        role: "user".into(),
        timestamp: None,
        content: "sensitive preference".into(),
        expires_at_ms: None,
        supersedes_id: None,
    }];
    service
        .add(AddRequest {
            request_id: "same-request".into(),
            messages: messages.clone(),
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();

    let persisted = service.store.source_documents()[0]
        .filters
        .get("request_fingerprint")
        .unwrap();
    assert!(persisted.starts_with("v2:"));
    assert!(!persisted.contains("sensitive preference"));

    let error = service
        .add(AddRequest {
            request_id: "same-request".into(),
            messages,
            user_id: "user-a".into(),
            session_id: "session-b".into(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("different content"));
}

#[test]
fn invalid_later_message_does_not_partially_add_request() {
    let mut service = service();
    let error = service
        .add(AddRequest {
            request_id: "atomic-request".into(),
            messages: vec![
                Message {
                    role: "user".into(),
                    timestamp: None,
                    content: "first valid message".into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                },
                Message {
                    role: "system".into(),
                    timestamp: None,
                    content: "invalid role".into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                },
            ],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("role"));
    assert!(service.store.source_documents().is_empty());
}

#[test]
fn search_reflects_writes_with_no_republish_step() {
    let mut service = service();
    service
        .add(AddRequest {
            request_id: "first".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "project codename zephyr".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();

    service
        .add(AddRequest {
            request_id: "second".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "database codename quartz".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-b".into(),
        })
        .unwrap();

    // One service, one search entry point: reads see the writes directly,
    // with no snapshot re-publish step in between.
    let response = service
        .search(SearchRequest {
            query: "database codename quartz".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(response
        .data
        .iter()
        .any(|memory| memory.content.contains("quartz")));
}

#[test]
fn expired_memories_are_not_searchable_and_can_be_deleted_idempotently() {
    let mut service = service();
    service
        .add(AddRequest {
            request_id: "request-expired".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "temporary migration decision".into(),
                expires_at_ms: Some(1),
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();
    let response = service
        .search(SearchRequest {
            query: "temporary migration".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(response.data.is_empty());
    assert!(!service.delete("user-a", "missing").unwrap());
}

#[test]
fn expired_high_ranked_memory_does_not_consume_top_k_slot() {
    let mut service = service();
    for (request_id, content, expires_at_ms) in [
        (
            "live",
            "phoenix deployment uses a stable canary rollout",
            None,
        ),
        (
            "expired",
            "phoenix deployment uses a stable canary rollout with stale archive marker",
            Some(1),
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: content.into(),
                    expires_at_ms,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "phoenix deployment stable canary rollout stale archive marker".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 1,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].id, memory_document_id("user-a", "live", 0));
}

#[test]
fn superseded_memory_is_hidden_but_replacement_remains_searchable() {
    let mut service = service();
    let old_id = memory_document_id("user-a", "old", 0);
    for (request_id, content, supersedes_id) in [
        ("old", "old deployment decision", None),
        ("new", "new deployment decision", Some(old_id.as_str())),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: supersedes_id.map(str::to_string),
                }],
                user_id: "user-a".into(),
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "deployment decision".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("new deployment"));
}

#[test]
fn semantic_supersession_hides_stale_doc_in_single_segment_corpus() {
    // Regression: the segmented query path dropped the semantic
    // supersession allow-list when the corpus fit in a single segment
    // (one session). The relations were computed correctly; the query
    // just never applied them.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    // Day-apart timestamps: chronological supersession needs strictly
    // greater dates (same calendar day does not count).
    for (request_id, content, timestamp) in [
        ("old", "timeout: 100", 1_699_939_200_000i64),
        ("new", "timeout: 150", 1_700_025_600_000i64),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(timestamp),
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                // Same session => single segment.
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "what is the timeout".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("timeout: 150"));
}

#[test]
fn decimal_config_backfill_hides_stale_value() {
    // End-to-end for the decimal sentence-splitting regression: the older
    // effective value (written later, as a backfill) must be superseded.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    for (request_id, content, timestamp) in [
        ("new", "version: 2.0", 1_700_003_200_000i64), // 2023-11-15
        ("old", "version: 1.5", 1_699_916_800_000i64), // 2023-11-14
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(timestamp),
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "what version are we on".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("version: 2.0"));
}

#[test]
fn correction_cue_supersedes_prior_usage_claim() {
    // End-to-end for the chain-key regression: "instead of" must not be
    // absorbed into the usage claim's subject.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    for (request_id, content, timestamp) in [
        (
            "old",
            "We use Postgres for analytics.",
            1_699_939_200_000i64, // 2023-11-14
        ),
        (
            "new",
            "We use MongoDB for analytics instead of Postgres.",
            1_700_025_600_000i64, // 2023-11-15
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(timestamp),
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "what database do we use for analytics".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("MongoDB"));
}

#[test]
fn correction_cue_supersedes_across_sessions() {
    // Probe P4 exact replication: the two docs live in DIFFERENT sessions
    // (hence different segments). The usage chain is user-scoped, so the
    // correction must still apply.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    for (request_id, content, timestamp, session_id) in [
        (
            "old",
            "We use Postgres for analytics.",
            1_699_939_200_000i64,
            "s1",
        ),
        (
            "new",
            "We use MongoDB for analytics instead of Postgres.",
            1_700_025_600_000i64,
            "s2",
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(timestamp),
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: session_id.into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "what database do we use for analytics".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("MongoDB"));
}

#[test]
fn add_response_serde_round_trip_and_backward_compat() {
    // Old JSON without the adjudication field must still deserialize.
    let old_json = r#"{"success":true,"request_id":"r1","user_id":"u","session_id":"s"}"#;
    let parsed: AddResponse = serde_json::from_str(old_json).unwrap();
    assert!(parsed.adjudication.is_none());

    // New responses round-trip, including the receipt.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    let response = service
        .add(AddRequest {
            request_id: "r1".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: Some(1_699_939_200_000i64),
                content: "The bicycle is owned by Rossi.".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "s1".into(),
        })
        .unwrap();
    let json = serde_json::to_value(&response).unwrap();
    assert!(json.get("adjudication").is_some());
    let back: AddResponse = serde_json::from_value(json).unwrap();
    assert_eq!(
        back.adjudication.unwrap().claims.len(),
        response.adjudication.unwrap().claims.len()
    );
}

#[test]
fn retry_after_restart_returns_identical_response() {
    // The identical-retry contract must survive restarts, intervening
    // writes, and multi-message requests: the persisted receipt is the
    // original, not a reconstruction from current state.
    let dir = canonical_temp_dir("receipt-restart");
    let _ = std::fs::remove_dir_all(&dir);
    let options = crate::default_production_pipeline_options();
    let args_a = || AddRequest {
        request_id: "rr-a".into(),
        messages: vec![
            Message {
                role: "user".into(),
                timestamp: Some(1_699_939_200_000i64),
                content: "The bicycle is owned by Rossi.".into(),
                expires_at_ms: None,
                supersedes_id: None,
            },
            Message {
                role: "user".into(),
                timestamp: Some(1_699_939_200_000i64),
                content: "The bicycle is red.".into(),
                expires_at_ms: None,
                supersedes_id: None,
            },
        ],
        user_id: "user-a".into(),
        session_id: "s1".into(),
    };
    let args_b = || AddRequest {
        request_id: "rr-b".into(),
        messages: vec![Message {
            role: "user".into(),
            timestamp: Some(1_700_025_600_000i64),
            content: "The bicycle is owned by Bianchi.".into(),
            expires_at_ms: None,
            supersedes_id: None,
        }],
        user_id: "user-a".into(),
        session_id: "s2".into(),
    };
    let first_json = {
        let mut service = MemoryService::at_path(&dir, options.clone()).expect("open failed");
        service.add(args_a()).expect("add A failed");
        // Intervening write supersedes A's ownership claim.
        service.add(args_b()).expect("add B failed");
        let retry_a = service.add(args_a()).expect("retry A failed");
        serde_json::to_value(&retry_a).expect("serialize failed")
    };
    // Reopen: the in-memory receipt cache is empty; the persisted
    // receipt must produce the identical serialized response.
    let mut service = MemoryService::at_path(&dir, options).expect("reopen failed");
    let retry_after_restart = service.add(args_a()).expect("retry failed");
    let second_json = serde_json::to_value(&retry_after_restart).expect("serialize failed");
    assert_eq!(first_json, second_json);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn batch_duplicate_requests_get_identical_receipts() {
    // [A, A] in one batch: the duplicate must resolve to the same
    // receipt as the original once the batch's refresh completes.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    let args = || AddRequest {
        request_id: "dup".into(),
        messages: vec![Message {
            role: "user".into(),
            timestamp: Some(1_699_939_200_000i64),
            content: "The bicycle is owned by Rossi.".into(),
            expires_at_ms: None,
            supersedes_id: None,
        }],
        user_id: "user-a".into(),
        session_id: "s1".into(),
    };
    let responses = service.add_batch(vec![args(), args()]).unwrap();
    assert_eq!(responses.len(), 2);
    let first = serde_json::to_value(&responses[0]).unwrap();
    let second = serde_json::to_value(&responses[1]).unwrap();
    assert_eq!(first, second);
    assert!(responses[0].adjudication.is_some());
}

#[test]
fn add_batch_receipts_attribute_cross_request_relations() {
    // In a batch, the second request's receipt must show the supersedes
    // decision against the first request's doc, and vice versa.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    let responses = service
        .add_batch(vec![
            AddRequest {
                request_id: "b1".into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(1_699_939_200_000i64),
                    content: "The bicycle is owned by Rossi.".into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "s1".into(),
            },
            AddRequest {
                request_id: "b2".into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(1_700_025_600_000i64),
                    content: "The bicycle is owned by Bianchi.".into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "s2".into(),
            },
        ])
        .unwrap();
    assert_eq!(responses.len(), 2);
    let first = responses[0].adjudication.as_ref().expect("receipt missing");
    let second = responses[1].adjudication.as_ref().expect("receipt missing");
    // Second write is the source of the supersedes relation.
    assert_eq!(second.decisions.len(), 1);
    assert_eq!(second.decisions[0].kind, "supersedes");
    assert!(second.decisions[0].is_source);
    assert_eq!(second.retired_doc_ids, first.doc_ids);
    // First write is the target side of the same relation.
    assert_eq!(first.decisions.len(), 1);
    assert_eq!(first.decisions[0].kind, "supersedes");
    assert!(!first.decisions[0].is_source);
    assert_eq!(first.decisions[0].other_doc_id, second.doc_ids[0]);
    assert!(first.retired_doc_ids.is_empty());
}

#[test]
fn add_response_carries_adjudication_receipt() {
    // The receipt must report what the write extracted and decided:
    // one claim, one supersedes decision, one retired doc.
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    let first = service
        .add(AddRequest {
            request_id: "r1".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: Some(1_699_939_200_000i64),
                content: "The bicycle is owned by Rossi.".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "s1".into(),
        })
        .unwrap();
    // First write: claim extracted, no clash decisions yet.
    let receipt = first.adjudication.as_ref().expect("receipt missing");
    assert_eq!(receipt.request_id, "r1");
    assert_eq!(receipt.doc_ids.len(), 1);
    assert_eq!(receipt.claims.len(), 1);
    assert_eq!(receipt.claims[0].object, "rossi");
    assert!(receipt.decisions.is_empty());
    assert!(receipt.retired_doc_ids.is_empty());
    let first_doc_ids = receipt.doc_ids.clone();

    let second = service
        .add(AddRequest {
            request_id: "r2".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: Some(1_700_025_600_000i64),
                content: "The bicycle is owned by Bianchi.".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "s2".into(),
        })
        .unwrap();
    let receipt = second.adjudication.expect("receipt missing");
    assert_eq!(receipt.claims.len(), 1);
    assert_eq!(receipt.claims[0].object, "bianchi");
    // One decision: this write supersedes the Rossi doc.
    assert_eq!(receipt.decisions.len(), 1);
    let decision = &receipt.decisions[0];
    assert_eq!(decision.kind, "supersedes");
    assert!(decision.is_source);
    assert_eq!(decision.method, "canonical_claim_and_time");
    // The retired doc is the first write's doc.
    assert_eq!(receipt.retired_doc_ids, first_doc_ids);
    assert_eq!(receipt.retired_doc_ids.len(), 1);
}

#[test]
fn correction_cue_supersedes_with_identical_timestamps() {
    // Probe P4 exact: SAME timestamp on both docs. The correction cue
    // alone must force supersession (chronological is false).
    let mut service = MemoryService::in_memory(crate::default_production_pipeline_options());
    for (request_id, content, session_id) in [
        ("old", "We use Postgres for analytics.", "s1"),
        (
            "new",
            "We use MongoDB for analytics instead of Postgres.",
            "s2",
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: Some(1_699_939_200_000i64),
                    content: content.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: session_id.into(),
            })
            .unwrap();
    }
    let response = service
        .search(SearchRequest {
            query: "which database is used for analytics".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("MongoDB"));
}

#[test]
fn supersession_is_scoped_to_the_requesting_user() {
    let mut service = service();
    let old_id = memory_document_id("user-a", "old", 0);
    service
        .add(AddRequest {
            request_id: "old".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "shared deployment decision".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();
    service
        .add(AddRequest {
            request_id: "replacement".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "replacement from another user".into(),
                expires_at_ms: None,
                supersedes_id: Some(old_id),
            }],
            user_id: "user-b".into(),
            session_id: "session-b".into(),
        })
        .unwrap();

    let response = service
        .search(SearchRequest {
            query: "shared deployment decision".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("shared deployment"));
}

#[test]
fn deleting_replacement_restores_predecessor_visibility_immediately() {
    let dir = canonical_temp_dir("delete-replacement-restores-predecessor");
    let mut service = MemoryService::at_path(&dir, PipelineOptions::default()).unwrap();
    let old_id = memory_document_id("alice", "old", 0);
    service
        .add(AddRequest {
            request_id: "old".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "the deployment uses blue-green releases".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "alice".into(),
            session_id: "session".into(),
        })
        .unwrap();
    let replacement_id = memory_document_id("alice", "replacement", 0);
    service
        .add(AddRequest {
            request_id: "replacement".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "the deployment uses canary releases".into(),
                expires_at_ms: None,
                supersedes_id: Some(old_id.clone()),
            }],
            user_id: "alice".into(),
            session_id: "session".into(),
        })
        .unwrap();

    assert!(service
        .get(GetRequest {
            user_id: "alice".into(),
            memory_id: old_id.clone(),
            include_inactive: false,
        })
        .unwrap()
        .is_none());
    assert!(service.delete("alice", &replacement_id).unwrap());
    assert!(service
        .get(GetRequest {
            user_id: "alice".into(),
            memory_id: old_id,
            include_inactive: false,
        })
        .unwrap()
        .is_some());

    drop(service);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn list_get_and_update_preserve_identity_and_scope() {
    let mut service = service();
    service
        .add(AddRequest {
            request_id: "request-1".into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: "old preference".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: "user-a".into(),
            session_id: "session-a".into(),
        })
        .unwrap();
    let listed = service
        .list(ListRequest {
            user_id: "user-a".into(),
            session_id: Some("session-a".into()),
            limit: 10,
            cursor: None,
            include_inactive: false,
        })
        .unwrap();
    assert_eq!(listed.data.len(), 1);
    let id = listed.data[0].id.clone();
    assert!(service
        .get(GetRequest {
            user_id: "user-b".into(),
            memory_id: id.clone(),
            include_inactive: false,
        })
        .unwrap()
        .is_none());

    let updated = service
        .update(UpdateRequest {
            user_id: "user-a".into(),
            memory_id: id.clone(),
            content: "new preference".into(),
            role: None,
            timestamp: None,
            expires_at_ms: None,
        })
        .unwrap()
        .unwrap();
    assert_eq!(updated.id, id);
    assert!(updated.content.contains("new preference"));
    assert_eq!(updated.session_id.as_deref(), Some("session-a"));

    let invalid_update = service.update(UpdateRequest {
        user_id: "user-a".into(),
        memory_id: id.clone(),
        content: "should not be applied".into(),
        role: None,
        timestamp: Some(i64::MAX),
        expires_at_ms: None,
    });
    assert!(invalid_update.is_err());
    let unchanged = service
        .get(GetRequest {
            user_id: "user-a".into(),
            memory_id: id,
            include_inactive: false,
        })
        .unwrap()
        .unwrap();
    assert!(unchanged.content.contains("new preference"));
}

#[test]
fn list_cursor_is_deterministic_and_excludes_inactive_memories() {
    let mut service = service();
    for request_id in ["a", "b"] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                messages: vec![Message {
                    role: "assistant".into(),
                    timestamp: None,
                    content: format!("memory {request_id}"),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: "session-a".into(),
            })
            .unwrap();
    }
    let first = service
        .list(ListRequest {
            user_id: "user-a".into(),
            session_id: None,
            limit: 1,
            cursor: None,
            include_inactive: false,
        })
        .unwrap();
    assert_eq!(first.data.len(), 1);
    let second = service
        .list(ListRequest {
            user_id: "user-a".into(),
            session_id: None,
            limit: 1,
            cursor: first.next_cursor,
            include_inactive: false,
        })
        .unwrap();
    assert_eq!(second.data.len(), 1);
    assert_ne!(first.data[0].id, second.data[0].id);
}

#[test]
fn group_id_is_an_exact_filter_only_when_explicitly_requested() {
    use std::collections::BTreeMap;

    let mut service = service();
    // Two sessions for the same user; group_id comes from session_id.
    for (session, text) in [
        (
            "session-one",
            "the quartz database uses append-only segments",
        ),
        (
            "session-two",
            "the quartz database uses append-only segments",
        ),
    ] {
        service
            .add(AddRequest {
                request_id: format!("req-{session}"),
                messages: vec![Message {
                    role: "user".into(),
                    timestamp: None,
                    content: text.into(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
                user_id: "user-a".into(),
                session_id: session.to_string(),
            })
            .unwrap();
    }

    // No group filter: both sessions' documents are searchable. A
    // session_id on the *search* is a state key only and must not
    // implicitly scope the corpus.
    let prepared = PreparedQuery::new("quartz database segments");
    let mut filters = BTreeMap::new();
    filters.insert(USER_FILTER.to_string(), "user-a".to_string());
    let unfiltered = service
        .store
        .query_prepared(&prepared, 10, &filters)
        .unwrap();
    assert_eq!(unfiltered.len(), 2);

    // Explicit group_id filter: only the requested session's documents.
    filters.insert("group_id".to_string(), "session-one".to_string());
    let filtered = service
        .store
        .query_prepared(&prepared, 10, &filters)
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].group_id.as_deref(), Some("session-one"));
}

#[test]
fn session_temporal_anchor_carries_into_follow_up_retrieval() {
    use chrono::Duration;

    let options = PipelineOptions {
        memory_index_layout: crate::pipeline::MemoryIndexLayout::Segmented {
            query_top_n: 8,
            routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
        },
        ..PipelineOptions::default()
    };
    let mut service = MemoryService::new(IndexStore::in_memory(options));
    let today = Utc::now().date_naive();
    let recent_date = (today - Duration::days(1)).format("%Y-%m-%d").to_string();
    let old_date = (today - Duration::days(30)).format("%Y-%m-%d").to_string();

    for (doc_id, group_id, timestamp, content) in [
        (
            "recent-doc",
            "recent",
            recent_date.as_str(),
            "Budget planning notes: allocate 20 percent to research.",
        ),
        (
            "old-doc",
            "old",
            old_date.as_str(),
            "Budget planning notes: allocate 5 percent to research.",
        ),
    ] {
        let mut doc_filters = BTreeMap::new();
        doc_filters.insert(USER_FILTER.to_string(), "user-a".to_string());
        service.store.upsert(SourceDocument {
            doc_id: doc_id.to_string(),
            source: format!("artifact://{doc_id}"),
            content: content.to_string(),
            concept: doc_id.to_string(),
            group_id: Some(group_id.to_string()),
            headings: vec![],
            links: vec![],
            timestamp: Some(timestamp.to_string()),
            doc_length: content.len(),
            author_agent: None,
            filters: doc_filters,
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        });
    }
    service.store.refresh().unwrap();

    // Turn 1 sets a temporal anchor via a relative phrase ("yesterday").
    service
        .search(SearchRequest {
            query: "What did we discuss yesterday?".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: Some("s-temporal".into()),
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();

    // Turn 2 is a follow-up: the carried anchor must restrict retrieval
    // to the in-window segment.
    let turn2 = service
        .search(SearchRequest {
            query: "what about the budget?".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: Some("s-temporal".into()),
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    let ids: Vec<&str> = turn2.data.iter().map(|memory| memory.id.as_str()).collect();
    assert!(
        ids.contains(&"recent-doc"),
        "follow-up should retrieve in-window doc, got {ids:?}"
    );
    assert!(
        !ids.contains(&"old-doc"),
        "follow-up should not retrieve out-of-window doc, got {ids:?}"
    );

    // Stateless baseline: the same wording without a session gets no
    // carried anchor, so the out-of-window segment is still searchable.
    let baseline = service
        .search(SearchRequest {
            query: "what about the budget?".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    let baseline_ids: Vec<&str> = baseline
        .data
        .iter()
        .map(|memory| memory.id.as_str())
        .collect();
    assert!(
        baseline_ids.contains(&"old-doc"),
        "stateless baseline should still see the out-of-window doc, got {baseline_ids:?}"
    );
}

fn add_quartz_fixture(service: &mut MemoryService) {
    for (doc_id, content) in [
            (
                "quartz-doc",
                "The Quartz database is a distributed store with strong consistency and tunable replication.",
            ),
            (
                "sourdough-doc",
                "Baking sourdough bread requires patience, a hot oven, and a lively starter.",
            ),
        ] {
            let mut doc_filters = BTreeMap::new();
            doc_filters.insert(USER_FILTER.to_string(), "user-a".to_string());
            service.store.upsert(SourceDocument {
                doc_id: doc_id.to_string(),
                source: format!("artifact://{doc_id}"),
                content: content.to_string(),
                concept: doc_id.to_string(),
                group_id: Some(doc_id.to_string()),
                headings: vec![],
                links: vec![],
                timestamp: None,
                doc_length: content.len(),
                author_agent: None,
                filters: doc_filters,
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            });
        }
    service.store.refresh().unwrap();
}

fn search(service: &mut MemoryService, query: &str, session_id: Option<&str>) -> SearchResponse {
    service
        .search(SearchRequest {
            query: query.into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: session_id.map(str::to_string),
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap()
}

#[test]
fn search_resolves_follow_up_against_session() {
    let mut service = service();
    add_quartz_fixture(&mut service);

    // Turn 1 establishes the session's entities.
    let first = search(
        &mut service,
        "Tell me about the Quartz database",
        Some("s1"),
    );
    assert!(first.data.iter().any(|m| m.content.contains("Quartz")));

    // A follow-up with no standalone meaning resolves against the session.
    let follow_up = search(&mut service, "what are its limitations?", Some("s1"));
    assert!(
        follow_up.data.iter().any(|m| m.content.contains("Quartz")),
        "follow-up should resolve against session state"
    );

    // Without a session the same query stays stateless and cannot borrow
    // the session's entities.
    let stateless = search(&mut service, "what are its limitations?", None);
    assert!(
        !stateless.data.iter().any(|m| m.content.contains("Quartz")),
        "stateless search must not resolve against session state"
    );
}

#[test]
fn search_rejects_blank_session_id() {
    let mut service = service();
    for session_id in [Some(""), Some("   ")] {
        let error = service
            .search(SearchRequest {
                query: "hello".into(),
                options: None,
                user_id: "user-a".into(),
                top_k: 10,
                session_id: session_id.map(str::to_string),
                scope: None,
                filters: None,
                reference_date: None,
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("session_id must not be empty"),
            "unexpected error: {error:#}"
        );
    }
}

#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "roo-runtime"
))]
#[test]
fn hook_query_uses_shared_search_and_contributes_follow_up_state() {
    // Enrichment disabled: this test exercises session state, and the
    // query-time backfill must not shell out to the real extractor here.
    let mut service = MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: false,
        ..PipelineOptions::default()
    });
    add_quartz_fixture(&mut service);

    // Hook-style retrieval: the query is not rewritten, but the turn is
    // recorded into the session state.
    let results = service
        .search_for_hook(
            "Tell me about the Quartz database",
            "claude",
            Some("hook-session"),
            10,
        )
        .unwrap();
    assert!(results.iter().any(|r| r.doc_id == "quartz-doc"));

    let hook_follow_up = service
        .search_for_hook(
            "what are its limitations?",
            "claude",
            Some("hook-session"),
            10,
        )
        .unwrap();
    assert!(
        hook_follow_up.iter().any(|r| r.doc_id == "quartz-doc"),
        "hook retrieval should resolve follow-ups through the shared search pipeline"
    );

    // A later MCP-style search in the same scope/session resolves the
    // follow-up against what the hook observed.
    let mut filters = BTreeMap::new();
    filters.insert(USER_FILTER.to_string(), "user-a".to_string());
    let follow_up = service
        .search_with_filters(
            "what are its limitations?",
            "claude",
            Some("hook-session"),
            10,
            &filters,
            None,
        )
        .unwrap();
    assert!(
        follow_up.iter().any(|r| r.doc_id == "quartz-doc"),
        "follow-up should resolve against hook-observed state"
    );

    // A blank hook session id degrades to stateless instead of failing.
    let blank = service
        .search_for_hook(
            "Tell me about the Quartz database",
            "claude",
            Some("  "),
            10,
        )
        .unwrap();
    assert!(blank.iter().any(|r| r.doc_id == "quartz-doc"));
}

#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "roo-runtime"
))]
#[test]
fn at_path_persists_conversation_state_across_instances() {
    let dir = canonical_temp_dir("conv-state");
    std::fs::create_dir_all(&dir).unwrap();
    // Enrichment disabled: this test exercises conversation state, and
    // the query-time backfill must not shell out to the real extractor.
    let options = PipelineOptions {
        key_phrase_enrichment: false,
        ..PipelineOptions::default()
    };
    {
        let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
        add_quartz_fixture(&mut service);
        service
            .search_for_hook(
                "Tell me about the Quartz database",
                "claude",
                Some("persist-s1"),
                10,
            )
            .unwrap();
    }
    // A fresh service against the same root reloads the session state
    // from disk, so the follow-up still resolves.
    let mut reopened = MemoryService::at_path(&dir, options).unwrap();
    let mut filters = BTreeMap::new();
    filters.insert(USER_FILTER.to_string(), "user-a".to_string());
    let follow_up = reopened
        .search_with_filters(
            "what are its limitations?",
            "claude",
            Some("persist-s1"),
            10,
            &filters,
            None,
        )
        .unwrap();
    assert!(
        follow_up.iter().any(|r| r.doc_id == "quartz-doc"),
        "follow-up should resolve against disk-persisted session state"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "roo-runtime"
))]
#[test]
fn compose_segmented_preserves_provider_conversation_state() {
    // Regression: compose_segmented() must not replace the disk-backed
    // conversation-state store with a memory-only one. Hooks write the
    // active-session pointer into the provider memory root; the composed
    // service has to see it, and explicit session state must survive a
    // new MCP process.
    let base = canonical_temp_dir("compose-state");
    let workspace_root = base.join("workspace-memory");
    let provider_root = base.join("memory");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::create_dir_all(&provider_root).unwrap();
    let options = crate::pipeline::PipelineOptions {
        memory_index_layout: crate::pipeline::MemoryIndexLayout::Segmented {
            query_top_n: 3,
            routing_strategy: crate::segments::SegmentRoutingStrategy::LocalDistinctiveness,
        },
        ..crate::pipeline::PipelineOptions::default()
    };
    {
        let provider = MemoryService::at_path(&provider_root, options.clone()).unwrap();
        // Simulate a hook invocation marking a session active.
        provider.note_active_session("claude", "sess-abc");
    }
    let workspace = MemoryService::at_path(&workspace_root, options.clone()).unwrap();
    let provider = MemoryService::at_path(&provider_root, options.clone()).unwrap();
    let composed = MemoryService::compose_segmented(workspace, Some(provider)).unwrap();
    assert_eq!(
        composed.current_session_id("claude"),
        Some("sess-abc".to_string()),
        "composed service must inherit the provider's conversation-state store"
    );

    // Without provider memory the composed service falls back to the
    // workspace store rather than a memory-only store.
    let workspace = MemoryService::at_path(&workspace_root, options.clone()).unwrap();
    workspace.note_active_session("claude", "sess-ws");
    let composed = MemoryService::compose_segmented(workspace, None).unwrap();
    assert_eq!(
        composed.current_session_id("claude"),
        Some("sess-ws".to_string()),
        "composed service must fall back to the workspace conversation-state store"
    );
    std::fs::remove_dir_all(&base).ok();
}

// --- Structured-fact production wiring ---

#[test]
fn temporal_participation_search_keeps_the_callers_reference_date() {
    let mut service = MemoryService::in_memory(PipelineOptions {
        ner_provider: Tier1NerProvider::Heuristic,
        ..PipelineOptions::default()
    });
    for (request_id, session_id, content, timestamp) in [
        (
            "triathlon",
            "session-triathlon",
            "I just completed the Spring Sprint Triathlon today.",
            1_685_664_000_000, // 2023-06-02 UTC
        ),
        (
            "five-k",
            "session-five-k",
            "I just finished the Midsummer 5K Run today.",
            1_686_355_200_000, // 2023-06-10 UTC
        ),
    ] {
        service
            .add(AddRequest {
                request_id: request_id.into(),
                user_id: "user-1".into(),
                session_id: session_id.into(),
                messages: vec![Message {
                    role: "user".into(),
                    content: content.into(),
                    timestamp: Some(timestamp),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
            })
            .expect("write event");
    }

    let response = service
            .search(SearchRequest {
                query: "What is the order of the three sports events I participated in during the past month, from earliest to latest?".into(),
                options: None,
                user_id: "user-1".into(),
                top_k: 5,
                session_id: None,
                scope: None,
                filters: None,
                reference_date: Some("2023/07/01".into()),
            })
            .expect("search events");

    let ordered_events: Vec<_> = response
        .data
        .iter()
        .filter_map(|hit| {
            hit.relation_evidence
                .first()
                .filter(|evidence| evidence.contains("participated_in"))
                .map(|evidence| evidence.as_str())
        })
        .collect();
    assert_eq!(
        ordered_events.len(),
        2,
        "structured events: {ordered_events:#?}"
    );
    assert!(ordered_events[0].contains("Spring Sprint Triathlon"));
    assert!(ordered_events[1].contains("Midsummer 5K Run"));

    let before_capture = service.temporal_facts_as_known_at("2023-06-10", "2000-01-01");
    assert!(before_capture.is_empty());
    let after_capture = service.temporal_facts_as_known_at("2023-06-10", "9999-01-01");
    assert!(
        !after_capture.is_empty(),
        "temporal facts should be queryable through MemoryService after their record time"
    );
}

use crate::segments::relations::{RawRelation, RelationIndex, RelationTurn};

fn structured_doc(
    doc_id: &str,
    content: &str,
    user: &str,
    extra_filters: &[(&str, &str)],
) -> SourceDocument {
    let mut filters = BTreeMap::new();
    filters.insert(USER_FILTER.to_string(), user.to_string());
    for (k, v) in extra_filters {
        filters.insert(k.to_string(), v.to_string());
    }
    SourceDocument {
        doc_id: doc_id.to_string(),
        source: format!("test/{doc_id}"),
        content: content.to_string(),
        concept: "test".to_string(),
        group_id: Some("sess-a".to_string()),
        headings: Vec::new(),
        links: Vec::new(),
        timestamp: None,
        doc_length: content.len(),
        author_agent: None,
        filters,
        key_phrases: Vec::new(),
        key_phrase_extraction_hash: String::new(),
    }
}

fn structured_store(docs: &[SourceDocument], options: PipelineOptions) -> IndexStore {
    IndexStore::with_documents(options, docs.to_vec())
}

/// Relation index where Gina and Jon both went to Rome (docs d1, d2).
fn rome_index() -> RelationIndex {
    let turns = ["Gina", "Jon"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| RelationTurn {
            speaker: s.to_string(),
            text: format!("{s} went to Rome"),
            session_id: "sess-a".to_string(),
            turn_idx: i,
            doc_id: format!("d{}", i + 1),
            session_date: None,
        })
        .collect::<Vec<_>>();
    let raw = ["Gina", "Jon"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| RawRelation {
            subject: s.to_string(),
            predicate: "go_to".to_string(),
            object: "Rome".to_string(),
            is_place: true,
            object_kind: "place".to_string(),
            is_activity: true,
            session_id: "sess-a".to_string(),
            turn_idx: i,
            doc_id: format!("d{}", i + 1),
            session_date: None,
            evidence: format!("{s}: went to Rome"),
            confidence: 0.9,
            coref: None,
        })
        .collect::<Vec<_>>();
    RelationIndex::build(&turns, &raw)
}

fn seeded_cache(
    user_id: &str,
    docs: &[&SourceDocument],
    index: RelationIndex,
) -> Mutex<RelationsCache> {
    let cache = Mutex::new(RelationsCache::default());
    {
        let mut guard = cache.lock().unwrap();
        let entry = guard.per_user.entry(user_id.to_string()).or_default();
        entry.fingerprint = relations_fingerprint(docs);
        entry.index = Some(Arc::new(index));
    }
    cache
}

/// Relation index where Gina and Jon both went to Paris (docs d5, d6).
fn paris_index() -> RelationIndex {
    let turns = ["Gina", "Jon"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| RelationTurn {
            speaker: s.to_string(),
            text: format!("{s} went to Paris"),
            session_id: "sess-b".to_string(),
            turn_idx: i,
            doc_id: format!("d{}", i + 5),
            session_date: None,
        })
        .collect::<Vec<_>>();
    let raw = ["Gina", "Jon"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| RawRelation {
            subject: s.to_string(),
            predicate: "go_to".to_string(),
            object: "Paris".to_string(),
            is_place: true,
            object_kind: "place".to_string(),
            is_activity: true,
            session_id: "sess-b".to_string(),
            turn_idx: i,
            doc_id: format!("d{}", i + 5),
            session_date: None,
            evidence: format!("{s}: went to Paris"),
            confidence: 0.9,
            coref: None,
        })
        .collect::<Vec<_>>();
    RelationIndex::build(&turns, &raw)
}

fn fact_request(query: &str, user: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        options: None,
        user_id: user.to_string(),
        top_k: 10,
        session_id: None,
        scope: None,
        filters: None,
        reference_date: None,
    }
}

#[test]
fn structured_results_return_evidence_hits() {
    let docs = vec![
        structured_doc("d1", "Gina: went to Rome", "u1", &[]),
        structured_doc("d2", "Jon: went to Rome", "u1", &[]),
    ];
    let refs: Vec<&SourceDocument> = docs.iter().collect();
    let store = structured_store(&docs, PipelineOptions::default());
    let cache = seeded_cache("u1", &refs, rome_index());
    let request = fact_request("Which city have both Gina and Jon visited?", "u1");
    let results = structured_fact_results(&store, &cache, &HashSet::new(), &request);
    let mut ids: Vec<&str> = results.iter().map(|r| r.doc_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["d1", "d2"]);
    for r in &results {
        assert!(r.score > 1000.0);
        assert_eq!(r.relation_evidence, vec!["shared relation: Rome"]);
        assert!(r.relation_confidence.is_some());
    }
}

#[test]
fn structured_results_enforce_visibility() {
    let expired = unix_time_ms().saturating_sub(1_000).to_string();
    let docs = vec![
        structured_doc("d1", "Gina: went to Rome", "u1", &[]),
        structured_doc("d2", "Jon: went to Rome", "u1", &[]),
        // Foreign user: must never compose into u1's relation index.
        structured_doc("d3", "Jon: went to Paris", "u2", &[]),
        // Superseded: filtered.
        structured_doc("d4", "Gina: went to Rome", "u1", &[]),
        // Expired: filtered.
        structured_doc(
            "d5",
            "Jon: went to Rome",
            "u1",
            &[("expires_at_ms", expired.as_str())],
        ),
    ];
    // The relation index is composed from u1's visible documents only;
    // d3 (foreign), d4 (superseded) and d5 (expired) contribute nothing.
    let visible: Vec<&SourceDocument> = docs[..2].iter().collect();
    let mut superseded = HashSet::new();
    superseded.insert(("u1".to_string(), "d4".to_string()));
    let request = fact_request("Which city have both Gina and Jon visited?", "u1");
    let cache = seeded_cache("u1", &visible, rome_index());
    let store = structured_store(&docs, PipelineOptions::default());
    let results = structured_fact_results(&store, &cache, &superseded, &request);
    let mut ids: Vec<&str> = results.iter().map(|r| r.doc_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["d1", "d2"],
        "only the visible, live, owned docs survive"
    );
    for r in &results {
        assert_eq!(r.relation_evidence, vec!["shared relation: Rome"]);
    }
    // A non-fact question never consults the index (no extractor cost).
    let plain = fact_request("Tell me about Rome.", "u1");
    assert!(structured_fact_results(&store, &cache, &HashSet::new(), &plain).is_empty());
    // The option flag disables the path entirely.
    let mut off = PipelineOptions::default();
    off.structured_fact_retrieval = false;
    let off_store = structured_store(&docs, off);
    assert!(structured_fact_results(&off_store, &cache, &HashSet::new(), &request).is_empty());
}

#[test]
fn structured_results_are_scoped_per_user() {
    let u1_docs = vec![
        structured_doc("d1", "Gina: went to Rome", "u1", &[]),
        structured_doc("d2", "Jon: went to Rome", "u1", &[]),
    ];
    let u2_docs = vec![
        structured_doc("d5", "Gina: went to Paris", "u2", &[]),
        structured_doc("d6", "Jon: went to Paris", "u2", &[]),
    ];
    let u1_refs: Vec<&SourceDocument> = u1_docs.iter().collect();
    let u2_refs: Vec<&SourceDocument> = u2_docs.iter().collect();
    let cache = Mutex::new(RelationsCache::default());
    {
        let mut guard = cache.lock().unwrap();
        let e1 = guard.per_user.entry("u1".to_string()).or_default();
        e1.fingerprint = relations_fingerprint(&u1_refs);
        e1.index = Some(Arc::new(rome_index()));
        let e2 = guard.per_user.entry("u2".to_string()).or_default();
        e2.fingerprint = relations_fingerprint(&u2_refs);
        e2.index = Some(Arc::new(paris_index()));
    }
    // Both users' documents reach the retrieval call; each user still
    // only ever consults their own relation index.
    let mut all: Vec<&SourceDocument> = Vec::new();
    all.extend(u1_refs.iter().copied());
    all.extend(u2_refs.iter().copied());
    let opts = PipelineOptions::default();
    let all_docs: Vec<SourceDocument> = all.into_iter().cloned().collect();
    let store = structured_store(&all_docs, opts);
    let r1 = structured_fact_results(
        &store,
        &cache,
        &HashSet::new(),
        &fact_request("Which city have both Gina and Jon visited?", "u1"),
    );
    let mut ids1: Vec<&str> = r1.iter().map(|r| r.doc_id.as_str()).collect();
    ids1.sort_unstable();
    assert_eq!(ids1, vec!["d1", "d2"]);
    assert!(r1
        .iter()
        .all(|r| r.relation_evidence == vec!["shared relation: Rome"]));
    let r2 = structured_fact_results(
        &store,
        &cache,
        &HashSet::new(),
        &fact_request("Which city have both Gina and Jon visited?", "u2"),
    );
    let mut ids2: Vec<&str> = r2.iter().map(|r| r.doc_id.as_str()).collect();
    ids2.sort_unstable();
    assert_eq!(ids2, vec!["d5", "d6"]);
    assert!(r2
        .iter()
        .all(|r| r.relation_evidence == vec!["shared relation: Paris"]));
}

fn scored_result(doc_id: &str, score: f32) -> crate::SearchResult {
    crate::SearchResult {
        doc_id: doc_id.to_string(),
        source: "test".to_string(),
        group_id: None,
        score,
        score_breakdown: crate::index::ScoreBreakdown::default(),
        matched_entities: Vec::new(),
        matched_terms: Vec::new(),
        probable_topic: None,
        doc_type_guess: None,
        semantic_status: None,
        superseded_by: None,
        relation_confidence: None,
        relation_evidence: Vec::new(),
        comparison_operands: Vec::new(),
    }
}

#[test]
fn blend_puts_structured_first_dedupes_and_respects_top_k() {
    let structured = vec![scored_result("d1", 1001.0)];
    let lexical = vec![
        scored_result("d1", 9.0), // duplicate: dropped
        scored_result("d2", 8.0),
        scored_result("d3", 7.0),
    ];
    let blended = blend_structured_first(structured, lexical, 2);
    let ids: Vec<&str> = blended.iter().map(|r| r.doc_id.as_str()).collect();
    assert_eq!(ids, vec!["d1", "d2"], "structured first, deduped, top_k=2");
    // Empty structured input returns the lexical list untouched.
    let lexical = vec![scored_result("d9", 5.0)];
    let blended = blend_structured_first(Vec::new(), lexical, 10);
    assert_eq!(blended.len(), 1);
    assert_eq!(blended[0].doc_id, "d9");
}

// ---------------- key-phrase enrichment ----------------

/// Fake extractor: one content-derived phrase per input turn, no spaCy.
/// `slow` keeps a batch "in flight" longer for the staleness test.
fn write_fake_extractor(name: &str, slow: bool) -> std::path::PathBuf {
    let dir = canonical_temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake_extractor.py");
    let delay = if slow { "0.3" } else { "0" };
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json, sys, time
time.sleep({delay})
payload = json.load(sys.stdin)
out = []
for t in payload.get("turns", []):
    text = t.get("text", "")
    marker = text[-12:] if len(text) >= 12 else text
    out.append({{
        "text": "phrase-" + marker,
        "kind": "thing",
        "session_id": t.get("session_id", ""),
        "doc_id": t.get("doc_id", ""),
        "turn_idx": t.get("turn_idx", 0),
    }})
json.dump({{"relations": [], "key_phrases": out}}, sys.stdout)
"#
        ),
    )
    .unwrap();
    script
}

fn enrichment_service(script: &std::path::Path) -> MemoryService {
    MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: true,
        // These tests exercise the spaCy extractor; opt out of the
        // Python-free default explicitly.
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(script.to_path_buf()),
        ..PipelineOptions::default()
    })
}

fn add_message(
    service: &mut MemoryService,
    request_id: &str,
    user_id: &str,
    session_id: &str,
    content: &str,
) {
    service
        .add(AddRequest {
            request_id: request_id.into(),
            messages: vec![Message {
                role: "user".into(),
                timestamp: None,
                content: content.into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
            user_id: user_id.into(),
            session_id: session_id.into(),
        })
        .unwrap();
}

static FLUSH_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn flush_id() -> String {
    let n = FLUSH_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("flush-{n}")
}

fn doc_id_for(user_id: &str, request_id: &str) -> String {
    memory_document_id(user_id, request_id, 0)
}

fn key_phrases_of(service: &MemoryService, doc_id: &str) -> Vec<String> {
    service
        .store
        .source_document_by_id(doc_id)
        .map(|d| d.key_phrases.iter().map(|p| p.text.clone()).collect())
        .unwrap_or_default()
}

/// Poll `cond` until it holds or `timeout` elapses.
fn poll_until(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cond()
}

#[test]
fn key_phrase_enrichment_backfills_documents_in_background() {
    let script = write_fake_extractor("enrich-fast", false);
    let mut service = enrichment_service(&script);
    add_message(
        &mut service,
        "req-1",
        "user-a",
        "sess-a",
        "I love the Paris jazz festival",
    );
    // The write returned immediately with no phrases; the worker needs
    // its linger plus one script run before a follow-up write applies
    // the inbox.
    let doc_id = doc_id_for("user-a", "req-1");
    assert!(key_phrases_of(&service, &doc_id).is_empty());
    let applied = poll_until(std::time::Duration::from_secs(15), || {
        add_message(&mut service, &flush_id(), "user-a", "sess-a", "flush");
        !key_phrases_of(&service, &doc_id).is_empty()
    });
    assert!(
        applied,
        "enriched phrases should land after a follow-up write"
    );
    let phrases = key_phrases_of(&service, &doc_id);
    assert_eq!(phrases.len(), 1);
    assert!(
        phrases[0].starts_with("phrase-"),
        "unexpected phrase: {}",
        phrases[0]
    );
    // Search still works after the backfill + refresh cycle.
    let response = service
        .search(SearchRequest {
            query: "jazz festival".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(response
        .data
        .iter()
        .any(|m| m.content.contains("jazz festival")));
}

#[test]
fn key_phrase_enrichment_fails_open_without_extractor() {
    let mut service = MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: true,
        // Test the spaCy fail-open path; opt out of Python-free default.
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(std::path::PathBuf::from("/nonexistent/extractor.py")),
        ..PipelineOptions::default()
    });
    add_message(
        &mut service,
        "req-1",
        "user-a",
        "sess-a",
        "I love the Paris jazz festival",
    );
    // Give the worker a chance to fail; the write path is unaffected.
    std::thread::sleep(std::time::Duration::from_secs(2));
    add_message(&mut service, "req-2", "user-a", "sess-a", "second note");
    assert!(key_phrases_of(&service, &doc_id_for("user-a", "req-1")).is_empty());
    let response = service
        .search(SearchRequest {
            query: "jazz festival".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert_eq!(response.data.len(), 1);
    assert!(response.data[0].content.contains("jazz festival"));
}

#[test]
fn key_phrase_enrichment_disabled_queues_nothing() {
    let script = write_fake_extractor("enrich-off", false);
    let mut service = MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: false,
        extractor_script: Some(script),
        ..PipelineOptions::default()
    });
    add_message(
        &mut service,
        "req-1",
        "user-a",
        "sess-a",
        "I love the Paris jazz festival",
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    add_message(&mut service, "req-2", "user-a", "sess-a", "second note");
    assert_eq!(service.enrichment_queue_len(), 0);
    assert!(key_phrases_of(&service, &doc_id_for("user-a", "req-1")).is_empty());
}

#[test]
fn key_phrase_enrichment_drops_stale_batches() {
    let script = write_fake_extractor("enrich-slow", true);
    let mut service = enrichment_service(&script);
    add_message(&mut service, "req-1", "user-a", "sess-a", "old content AAA");
    let doc_id = doc_id_for("user-a", "req-1");
    // The worker drains the queue after its 500ms linger; the slow
    // script keeps that batch in flight, so this update lands mid-flight.
    std::thread::sleep(std::time::Duration::from_millis(700));
    service
        .update(UpdateRequest {
            user_id: "user-a".into(),
            memory_id: doc_id.clone(),
            content: "new content BBB".into(),
            role: None,
            timestamp: None,
            expires_at_ms: None,
        })
        .unwrap();
    // Flush until the re-queued (new-content) batch lands. The stale
    // batch's phrases must never be applied: the marker is
    // content-derived, so old phrases would contain "AAA".
    let settled = poll_until(std::time::Duration::from_secs(25), || {
        add_message(&mut service, &flush_id(), "user-a", "sess-a", "flush");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        !key_phrases_of(&service, &doc_id).is_empty()
    });
    assert!(settled, "re-queued enrichment should land");
    let phrases = key_phrases_of(&service, &doc_id);
    assert!(
        phrases.iter().any(|p| p.contains("BBB")),
        "phrases should describe the new content: {phrases:?}"
    );
    assert!(
        phrases.iter().all(|p| !p.contains("AAA")),
        "stale phrases must be dropped: {phrases:?}"
    );
}

#[test]
fn key_phrase_enrichment_stress_concurrent_writes_and_reads() {
    use std::sync::{Arc, RwLock};
    let script = write_fake_extractor("enrich-stress", false);
    let service = Arc::new(RwLock::new(enrichment_service(&script)));
    let writers: Vec<_> = (0..6)
        .map(|w| {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                for i in 0..40 {
                    let mut guard = service.write().unwrap();
                    guard
                        .add(AddRequest {
                            request_id: format!("stress-{w}-{i}"),
                            messages: vec![Message {
                                role: "user".into(),
                                timestamp: None,
                                content: format!("writer {w} note number {i} about topology"),
                                expires_at_ms: None,
                                supersedes_id: None,
                            }],
                            user_id: "user-a".into(),
                            session_id: format!("sess-{w}"),
                        })
                        .unwrap();
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                for _ in 0..60 {
                    let mut guard = service.write().unwrap();
                    let _ = guard.search(SearchRequest {
                        query: "topology".into(),
                        options: None,
                        user_id: "user-a".into(),
                        top_k: 5,
                        session_id: None,
                        scope: None,
                        filters: None,
                        reference_date: None,
                    });
                    drop(guard);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    for r in readers {
        r.join().unwrap();
    }
    // Flush the enrichment pipeline: each flush write drains the inbox,
    // and the 1.2s pause lets the worker finish its batch.
    let mut guard = service.write().unwrap();
    let all_doc_ids: Vec<String> = (0..6)
        .flat_map(|w| (0..40).map(move |i| doc_id_for("user-a", &format!("stress-{w}-{i}"))))
        .collect();
    let mut done = false;
    for _ in 0..25 {
        add_message(&mut guard, &flush_id(), "user-a", "sess-flush", "flush");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        if all_doc_ids
            .iter()
            .all(|id| !key_phrases_of(&guard, id).is_empty())
        {
            done = true;
            break;
        }
    }
    assert!(done, "all 240 stress docs should be enriched");
    assert!(guard.enrichment_queue_len() <= KEY_PHRASE_PENDING_MAX);
    // Spot-check search quality after the backfill storm.
    let response = guard
        .search(SearchRequest {
            query: "topology".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(!response.data.is_empty());
}

// ------------------------------------------------------------------
// Query-time key-phrase backfill coverage.
//
// Provider hooks (Claude Code, Codex, Gemini) run as short-lived
// processes: they upsert, refresh, and exit before the background
// enrichment worker's linger completes. These tests cover the
// synchronous backfill that closes that gap.
// ------------------------------------------------------------------

/// Fake extractor emitting one fixed phrase per turn. The phrase is a
/// content mention (like real extractor output), so tests put it in the
/// document text.
fn write_canary_extractor(name: &str) -> std::path::PathBuf {
    let dir = canonical_temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("canary_extractor.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
out = []
for t in payload.get("turns", []):
    out.append({
        "text": "canary phrase",
        "kind": "thing",
        "session_id": t.get("session_id", ""),
        "doc_id": t.get("doc_id", ""),
        "turn_idx": t.get("turn_idx", 0),
    })
json.dump({"relations": [], "key_phrases": out}, sys.stdout)
"#,
    )
    .unwrap();
    script
}

/// Fake extractor that runs clean but finds no phrases: a legitimate
/// empty result, distinct from a failed run.
fn write_empty_extractor(name: &str) -> std::path::PathBuf {
    let dir = canonical_temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("empty_extractor.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys
json.load(sys.stdin)
json.dump({"relations": [], "key_phrases": []}, sys.stdout)
"#,
    )
    .unwrap();
    script
}

fn extractor_service(script: &std::path::Path) -> MemoryService {
    MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: true,
        // These tests exercise the spaCy extractor; opt out of the
        // Python-free default explicitly.
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(script.to_path_buf()),
        ..PipelineOptions::default()
    })
}

/// A document written the way a dead hook process leaves it: present
/// in the store with no phrases and no extraction stamp.
fn direct_doc(doc_id: &str, content: &str) -> SourceDocument {
    SourceDocument {
        doc_id: doc_id.to_string(),
        source: format!("test://{doc_id}"),
        content: content.to_string(),
        concept: String::new(),
        group_id: None,
        headings: vec![],
        links: vec![],
        timestamp: None,
        doc_length: content.len(),
        author_agent: None,
        filters: BTreeMap::new(),
        key_phrases: Vec::new(),
        key_phrase_extraction_hash: String::new(),
    }
}

fn enrichment(doc: &SourceDocument, phrase: &str) -> crate::source::SourceDocumentEnrichment {
    crate::source::SourceDocumentEnrichment {
        doc_id: doc.doc_id.clone(),
        expected_source_hash: crate::source::source_document_hash(doc).unwrap(),
        key_phrases: vec![crate::source::KeyPhrase {
            text: phrase.to_string(),
            kind: "thing".to_string(),
            origin: None,
        }],
        session_conclusion: None,
        headings: Vec::new(),
        links: Vec::new(),
        supersedes_document_id: None,
        evidence: Vec::new(),
    }
}

fn search_feedback_temp_dir(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("membrick-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn resident_search_review_state_loads_once_and_survives_restart() {
    let root = search_feedback_temp_dir("review-state-cache");
    let state_path = root.join(".membrick/search-review-state.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    write_search_review_state(
        &state_path,
        &serde_json::json!({
            "schema_version": 1,
            "reviews": {"existing-search": {"status": "reviewed"}}
        }),
    )
    .unwrap();

    let service = MemoryService::in_memory(PipelineOptions::default());
    assert_eq!(
        service
            .search_review_entry(&root, "existing-search")
            .unwrap()
            .unwrap()["status"],
        "reviewed"
    );

    // Once loaded, page lookups use the resident snapshot instead of
    // rereading the file. The resident remains the sole state owner.
    write_search_review_state(
        &state_path,
        &serde_json::json!({"schema_version": 1, "reviews": {}}),
    )
    .unwrap();
    assert!(service
        .search_review_entry(&root, "existing-search")
        .unwrap()
        .is_some());
    service
        .set_search_review_entry(
            &root,
            "new-search",
            serde_json::json!({"status": "reviewed"}),
        )
        .unwrap();

    let persisted = read_search_review_state(&state_path).unwrap();
    assert!(persisted["reviews"].get("existing-search").is_some());
    assert!(persisted["reviews"].get("new-search").is_some());

    let restarted = MemoryService::in_memory(PipelineOptions::default());
    assert!(restarted
        .search_review_entry(&root, "new-search")
        .unwrap()
        .is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dashboard_search_reviews_join_feedback_and_paginate_newest_first() {
    let root = search_feedback_temp_dir("dashboard-feedback");
    let directory = root.join(".membrick");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("mcp-call-history.jsonl"),
        vec![b'x'; 4_000_000],
    )
    .unwrap();
    std::fs::write(
            directory.join("search-feedback-dashboard.jsonl"),
            "{\"_snapshot\":{\"schema_version\":1,\"generated_at\":123,\"reviewed_searches\":2,\"evaluation\":{\"sample_status\":\"small\"}}}\n{\"query\":\"newer query\",\"provider\":\"codex\",\"label\":\"helpful\",\"document_judgments\":[{\"doc_id\":\"doc:one\",\"relevance\":\"relevant\"}]}\n{\"query\":\"older query\",\"provider\":\"codex\",\"label\":\"helpful\",\"document_judgments\":[{\"doc_id\":\"doc:two\",\"relevance\":\"relevant\"}]}\n",
        ).unwrap();

    let first = MemoryService::dashboard_search_feedback_reviews(&root, 0, 1).unwrap();
    assert_eq!(first["total"], 2);
    assert_eq!(first["next_offset"], 1);
    assert_eq!(first["reviews"][0]["query"], "newer query");
    assert_eq!(first["reviews"][0]["provider"], "codex");
    assert_eq!(first["reviews"][0]["label"], "helpful");
    assert_eq!(
        first["reviews"][0]["document_judgments"][0]["doc_id"],
        "doc:one"
    );

    let second = MemoryService::dashboard_search_feedback_reviews(&root, 1, 1).unwrap();
    assert_eq!(second["reviews"][0]["query"], "older query");
    assert_eq!(second["next_offset"], serde_json::Value::Null);
    let past_end = MemoryService::dashboard_search_feedback_reviews(&root, 20, 10).unwrap();
    assert_eq!(past_end["offset"], 2);
    assert!(past_end["reviews"].as_array().unwrap().is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dashboard_search_reviews_rejects_oversized_snapshot_rows_instead_of_skipping_them() {
    let root = search_feedback_temp_dir("dashboard-feedback-oversized-row");
    let directory = root.join(".membrick");
    std::fs::create_dir_all(&directory).unwrap();
    let oversized = serde_json::json!({"query":"x".repeat(600 * 1024)});
    let rows = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({"_snapshot":{"schema_version":1,"generated_at":123,"reviewed_searches":2,"evaluation":{}}}),
        oversized,
        serde_json::json!({"query":"second row"})
    );
    std::fs::write(directory.join("search-feedback-dashboard.jsonl"), rows).unwrap();

    let result = MemoryService::dashboard_search_feedback_reviews(&root, 0, 1);
    assert!(
        result.is_err(),
        "oversized snapshot row must be reported, not silently skipped"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dashboard_search_reviews_rejects_truncated_snapshot_instead_of_repeating_cursor() {
    let root = search_feedback_temp_dir("dashboard-feedback-truncated");
    let directory = root.join(".membrick");
    std::fs::create_dir_all(&directory).unwrap();
    let snapshot = format!(
        "{}\n{}\n",
        serde_json::json!({"_snapshot":{"schema_version":1,"generated_at":123,"reviewed_searches":2,"evaluation":{}}}),
        serde_json::json!({"query":"only row"})
    );
    std::fs::write(directory.join("search-feedback-dashboard.jsonl"), snapshot).unwrap();

    let result = MemoryService::dashboard_search_feedback_reviews(&root, 1, 1);
    assert!(
        result.is_err(),
        "truncated snapshot must not return a repeated next offset"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dashboard_search_reviews_reports_missing_offline_snapshot() {
    let root = search_feedback_temp_dir("dashboard-feedback-empty");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let result = MemoryService::dashboard_search_feedback_reviews(&root, 0, 25).unwrap();
    assert_eq!(result["total"], 0);
    assert_eq!(result["reviews"], serde_json::json!([]));
    assert_eq!(result["next_offset"], serde_json::Value::Null);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_document_enrichment_is_version_checked_and_retry_safe() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let document = direct_doc("enrich-target", "The command interface is documented here.");
    service.upsert(document.clone());
    let reviewed = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    let request = enrichment(&reviewed, "command interface");

    assert!(service.enrich_source_document(request.clone()).unwrap());
    assert!(!service.enrich_source_document(request).unwrap());
    let enriched = service.source_document_by_id(&document.doc_id).unwrap();
    assert_eq!(enriched.key_phrases.len(), 1);
    assert_eq!(
        enriched.key_phrases[0].origin,
        Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
    );

    let mut stale = enrichment(&reviewed, "stale phrase");
    stale.expected_source_hash = "incorrect-version".to_string();
    assert!(service.enrich_source_document(stale).is_err());
    assert_eq!(
        service
            .source_document_by_id(&document.doc_id)
            .unwrap()
            .key_phrases
            .len(),
        1
    );
}

#[test]
fn evidence_grounded_enrichment_survives_only_while_evidence_content_is_current() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let target = direct_doc("enrich-dependency-target", "The current CLI is documented.");
    let evidence = direct_doc(
        "enrich-dependency-evidence",
        "The command is named `membrick`.",
    );
    service.upsert(target.clone());
    service.upsert(evidence.clone());
    let reviewed_target = service
        .source_document_by_id(&target.doc_id)
        .unwrap()
        .clone();
    let reviewed_evidence = service
        .source_document_by_id(&evidence.doc_id)
        .unwrap()
        .clone();
    let mut proposal = enrichment(&reviewed_target, "project CLI");
    proposal.evidence = vec![crate::source::SourceDocumentEvidence {
        doc_id: evidence.doc_id.clone(),
        expected_source_hash: crate::source::source_document_hash(&reviewed_evidence).unwrap(),
    }];
    let prepared = service
        .prepare_source_document_enrichments(vec![proposal], Some("search-dependency"))
        .unwrap();
    service
        .apply_prepared_source_document_enrichments(prepared)
        .unwrap();
    service.flush().unwrap();

    let mut unchanged_target = target.clone();
    service.preserve_document_enrichment(
        MemoryStore::Captured,
        std::slice::from_mut(&mut unchanged_target),
    );
    assert!(unchanged_target.key_phrases.iter().any(|phrase| {
        phrase.text == "project CLI"
            && phrase.origin == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
    }));
    assert!(unchanged_target
        .filters
        .keys()
        .any(|key| key.starts_with(ENRICHMENT_CONTRIBUTION_PREFIX)));

    let mut revised_evidence = evidence;
    revised_evidence.content = "The command has been renamed to `membrickctl`.".into();
    revised_evidence.doc_length = revised_evidence.content.len();
    service.upsert(revised_evidence);
    let mut changed_target = target;
    service.preserve_document_enrichment(
        MemoryStore::Captured,
        std::slice::from_mut(&mut changed_target),
    );
    assert!(!changed_target
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "project CLI"));
    assert!(!changed_target
        .filters
        .keys()
        .any(|key| key.starts_with(ENRICHMENT_CONTRIBUTION_PREFIX)));

    let deleted_target = direct_doc(
        "enrich-deleted-dependency-target",
        "The command setup is documented.",
    );
    let deleted_evidence = direct_doc(
        "enrich-deleted-dependency-evidence",
        "The setup command is `membrick install`.",
    );
    service.upsert(deleted_target.clone());
    service.upsert(deleted_evidence.clone());
    let reviewed_target = service
        .source_document_by_id(&deleted_target.doc_id)
        .unwrap()
        .clone();
    let reviewed_evidence = service
        .source_document_by_id(&deleted_evidence.doc_id)
        .unwrap()
        .clone();
    let mut proposal = enrichment(&reviewed_target, "setup command");
    proposal.evidence = vec![crate::source::SourceDocumentEvidence {
        doc_id: deleted_evidence.doc_id.clone(),
        expected_source_hash: crate::source::source_document_hash(&reviewed_evidence).unwrap(),
    }];
    let prepared = service
        .prepare_source_document_enrichments(vec![proposal], Some("search-deleted-evidence"))
        .unwrap();
    service
        .apply_prepared_source_document_enrichments(prepared)
        .unwrap();
    service.flush().unwrap();
    service.store.remove(&deleted_evidence.doc_id);

    let mut after_delete = deleted_target;
    service.preserve_document_enrichment(
        MemoryStore::Captured,
        std::slice::from_mut(&mut after_delete),
    );
    assert!(!after_delete
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "setup command"));
    assert!(!after_delete
        .filters
        .keys()
        .any(|key| key.starts_with(ENRICHMENT_CONTRIBUTION_PREFIX)));
}

#[test]
fn provider_key_phrase_enrichment_changes_search_without_rewriting_source() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let document = direct_doc(
        "enrich-searchable",
        "An internal procedure is documented in this source.",
    );
    let original_content = document.content.clone();
    service.upsert(document.clone());
    let filters = BTreeMap::new();
    let query = "quartz command interface";
    assert!(service
        .search_with_filters(query, "user-a", None, 10, &filters, None)
        .unwrap()
        .iter()
        .all(|hit| hit.doc_id != document.doc_id));

    let reviewed = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    let update = enrichment(&reviewed, query);
    assert!(service.enrich_source_document(update.clone()).unwrap());
    assert!(!service.enrich_source_document(update).unwrap());

    let hits = service
        .search_with_filters(query, "user-a", None, 10, &filters, None)
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.doc_id == document.doc_id),
        "the provider phrase should become searchable after publication"
    );
    let enriched = service.source_document_by_id(&document.doc_id).unwrap();
    assert_eq!(enriched.content, original_content);
    assert_eq!(enriched.key_phrases.len(), 1);
    assert_eq!(
        enriched.key_phrases[0].origin,
        Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
    );
}

#[test]
fn segmented_feedback_enrichment_rebuilds_only_its_segment_and_matches_full_rebuild() {
    let options = PipelineOptions {
        memory_index_layout: crate::pipeline::MemoryIndexLayout::Segmented {
            query_top_n: 8,
            routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
        },
        ..PipelineOptions::default()
    };
    let mut service = MemoryService::in_memory(options.clone());
    let mut target = direct_doc(
        "enrich-segment-target",
        "The internal rollout procedure is recorded here.",
    );
    target.group_id = Some("session-target".to_string());
    let mut unrelated = direct_doc(
        "enrich-segment-unrelated",
        "A separate guide covers database maintenance.",
    );
    unrelated.group_id = Some("session-unrelated".to_string());
    service.upsert(target.clone());
    service.upsert(unrelated.clone());
    service.refresh().unwrap();

    let unrelated_segment_before = match service
        .store
        .memory_index_snapshot()
        .expect("initial segmented snapshot")
    {
        crate::pipeline::MemoryIndexSnapshot::Segmented(snapshot) => snapshot
            .segments
            .iter()
            .find(|segment| segment.doc_ids.contains(&unrelated.doc_id))
            .expect("unrelated document segment")
            .index
            .clone(),
        crate::pipeline::MemoryIndexSnapshot::Single(_) => {
            panic!("configured segmented layout produced a single index")
        }
    };

    let query = "quartz sequoia alignment";
    assert!(service
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .iter()
        .all(|hit| hit.doc_id != target.doc_id));
    let reviewed = service
        .store
        .source_document_by_id(&target.doc_id)
        .unwrap()
        .clone();
    service
        .enrich_source_document(enrichment(&reviewed, query))
        .unwrap();
    service.refresh().unwrap();

    let enriched_segmented = service
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    assert!(enriched_segmented.contains(&target.doc_id));
    match service
        .store
        .memory_index_snapshot()
        .expect("published enriched snapshot")
    {
        crate::pipeline::MemoryIndexSnapshot::Segmented(snapshot) => {
            let unrelated_segment_after = snapshot
                .segments
                .iter()
                .find(|segment| segment.doc_ids.contains(&unrelated.doc_id))
                .expect("unrelated document segment")
                .index
                .clone();
            assert!(
                std::sync::Arc::ptr_eq(&unrelated_segment_before, &unrelated_segment_after),
                "enrichment should leave unrelated segment indexes shared"
            );
        }
        crate::pipeline::MemoryIndexSnapshot::Single(_) => {
            panic!("enrichment changed the configured index layout")
        }
    }

    let enriched_target = service
        .store
        .source_document_by_id(&target.doc_id)
        .unwrap()
        .clone();
    let mut full_rebuild = MemoryService::in_memory(options);
    full_rebuild.upsert(enriched_target);
    full_rebuild.upsert(unrelated);
    full_rebuild.refresh().unwrap();
    let rebuilt_results = full_rebuild
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    assert_eq!(
        enriched_segmented, rebuilt_results,
        "incremental enrichment should match a full rebuild for the same sources"
    );
}

#[test]
fn segmented_feedback_supersession_matches_full_rebuild_across_segments() {
    let options = PipelineOptions {
        memory_index_layout: crate::pipeline::MemoryIndexLayout::Segmented {
            query_top_n: 8,
            routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
        },
        ..PipelineOptions::default()
    };
    let mut service = MemoryService::in_memory(options.clone());
    let mut old = direct_doc(
        "supersession-segment-old",
        "The deployment routing policy uses the blue gateway.",
    );
    old.group_id = Some("supersession-old-session".to_string());
    old.filters
        .insert("semantic_scope".into(), "project:segmented-feedback".into());
    let mut replacement = direct_doc(
        "supersession-segment-new",
        "The deployment routing policy now uses the green gateway.",
    );
    replacement.group_id = Some("supersession-new-session".to_string());
    replacement
        .filters
        .insert("semantic_scope".into(), "project:segmented-feedback".into());
    let mut unrelated = direct_doc(
        "supersession-segment-unrelated",
        "A separate handbook describes database backups.",
    );
    unrelated.group_id = Some("supersession-unrelated-session".to_string());
    service.upsert(old.clone());
    service.upsert(replacement.clone());
    service.upsert(unrelated.clone());
    service.refresh().unwrap();

    let query = "deployment routing policy gateway";
    let before = service
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    assert!(before.contains(&old.doc_id));
    assert!(before.contains(&replacement.doc_id));

    let unrelated_segment_before = match service
        .store
        .memory_index_snapshot()
        .expect("initial segmented snapshot")
    {
        crate::pipeline::MemoryIndexSnapshot::Segmented(snapshot) => snapshot
            .segments
            .iter()
            .find(|segment| segment.doc_ids.contains(&unrelated.doc_id))
            .expect("unrelated document segment")
            .index
            .clone(),
        crate::pipeline::MemoryIndexSnapshot::Single(_) => {
            panic!("configured segmented layout produced a single index")
        }
    };

    let reviewed_replacement = service
        .store
        .source_document_by_id(&replacement.doc_id)
        .unwrap()
        .clone();
    let mut update = enrichment(&reviewed_replacement, "deployment routing policy");
    update.supersedes_document_id = Some(old.doc_id.clone());
    service.enrich_source_document(update).unwrap();
    service.refresh().unwrap();

    let incremental_results = service
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    assert!(incremental_results.contains(&replacement.doc_id));
    assert!(!incremental_results.contains(&old.doc_id));
    assert_eq!(
        service.semantic_document_state(&old.doc_id).status,
        Some(crate::semantic_relations::SemanticStatus::Superseded)
    );

    match service
        .store
        .memory_index_snapshot()
        .expect("published supersession snapshot")
    {
        crate::pipeline::MemoryIndexSnapshot::Segmented(snapshot) => {
            let unrelated_segment_after = snapshot
                .segments
                .iter()
                .find(|segment| segment.doc_ids.contains(&unrelated.doc_id))
                .expect("unrelated document segment")
                .index
                .clone();
            assert!(
                std::sync::Arc::ptr_eq(&unrelated_segment_before, &unrelated_segment_after),
                "semantic supersession should not rebuild an unrelated segment"
            );
        }
        crate::pipeline::MemoryIndexSnapshot::Single(_) => {
            panic!("supersession changed the configured index layout")
        }
    }

    let updated_replacement = service
        .store
        .source_document_by_id(&replacement.doc_id)
        .unwrap()
        .clone();
    let mut full_rebuild = MemoryService::in_memory(options);
    full_rebuild.upsert(old);
    full_rebuild.upsert(updated_replacement);
    full_rebuild.upsert(unrelated);
    full_rebuild.refresh().unwrap();
    let rebuilt_results = full_rebuild
        .search_with_filters(query, "user-a", None, 10, &BTreeMap::new(), None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    assert_eq!(
        incremental_results, rebuilt_results,
        "incremental semantic publication should match a full rebuild"
    );
}

#[test]
fn provider_enrichments_reject_stale_writes_and_preserve_both_additions() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let document = direct_doc("enrich-concurrent", "Shared project documentation.");
    service.upsert(document.clone());
    let reviewed = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();

    // Two providers prepare work from the same source version. The first
    // update wins; the stale second update must surface a conflict instead
    // of replacing the first provider's contribution.
    service
        .enrich_source_document(enrichment(&reviewed, "claude terminology"))
        .unwrap();
    let stale_result = service.enrich_source_document(enrichment(&reviewed, "codex terminology"));
    assert!(stale_result
        .unwrap_err()
        .to_string()
        .contains("changed after enrichment"));
    let after_conflict = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    assert!(after_conflict
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "claude terminology"));
    assert!(!after_conflict
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "codex terminology"));

    // The second provider can refresh its reviewed hash and merge its
    // additive phrase without removing the first contribution.
    service
        .enrich_source_document(enrichment(&after_conflict, "codex terminology"))
        .unwrap();
    let merged = service.source_document_by_id(&document.doc_id).unwrap();
    assert!(merged
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "claude terminology"));
    assert!(merged
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "codex terminology"));
}

#[test]
fn session_context_enrichment_survives_later_hook_capture() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let observation = direct_doc("session-observation-one", "The resident owns writes.");
    let mut context = direct_doc("session-context-one", "Session session-one context.");
    context
        .filters
        .insert("document_type".into(), "session-context".into());
    context
        .filters
        .insert("session_id".into(), "session-one".into());
    context.links.push(observation.doc_id.clone());
    service.upsert(observation.clone());
    service.upsert(context.clone());

    let reviewed = service
        .source_document_by_id(&context.doc_id)
        .unwrap()
        .clone();
    let mut update = enrichment(&reviewed, "resident writer");
    update.session_conclusion = Some("The resident serializes durable writes.".into());
    update.headings.push("Writer architecture".into());
    assert_eq!(
        service
            .enrich_source_documents_for_operation(vec![update], Some("search-123"))
            .unwrap()
            .len(),
        1
    );

    let next_observation = direct_doc(
        "session-observation-two",
        "Searches use the published view.",
    );
    service.upsert(next_observation.clone());
    let mut next_context = context;
    next_context.links = vec![next_observation.doc_id.clone()];
    service.preserve_document_enrichment(
        MemoryStore::Captured,
        std::slice::from_mut(&mut next_context),
    );
    assert!(next_context
        .content
        .contains("The resident serializes durable writes."));
    assert!(next_context
        .headings
        .contains(&"Writer architecture".into()));
    assert!(next_context.links.contains(&observation.doc_id));
    assert!(next_context.links.contains(&next_observation.doc_id));
    service.upsert(next_context);
    let persisted = service.source_document_by_id(&reviewed.doc_id).unwrap();
    assert!(persisted
        .content
        .contains("The resident serializes durable writes."));
    assert_eq!(
        persisted
            .filters
            .get("membrick_session_conclusion")
            .map(String::as_str),
        Some("The resident serializes durable writes.")
    );
}

#[test]
fn enrichment_batch_validates_before_staging_any_document() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let first = direct_doc("enrich-batch-first", "first source text");
    let second = direct_doc("enrich-batch-second", "second source text");
    service.upsert(first.clone());
    service.upsert(second.clone());
    let reviewed_first = service
        .source_document_by_id(&first.doc_id)
        .unwrap()
        .clone();
    let reviewed_second = service
        .source_document_by_id(&second.doc_id)
        .unwrap()
        .clone();
    let mut stale_second = enrichment(&reviewed_second, "stale addition");
    stale_second.expected_source_hash = "stale-version".to_string();

    let error = service
        .enrich_source_documents(vec![
            enrichment(&reviewed_first, "first new phrase"),
            stale_second,
        ])
        .unwrap_err();
    assert!(error.to_string().contains("changed after enrichment"));
    assert!(service
        .source_document_by_id(&first.doc_id)
        .unwrap()
        .key_phrases
        .is_empty());
    assert!(service
        .source_document_by_id(&second.doc_id)
        .unwrap()
        .key_phrases
        .is_empty());
}

#[test]
fn enrich_doc_targets_one_document_and_checks_exact_evidence_revision() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let target = direct_doc(
        "enrich-doc-target",
        "The shared MCP query uses the resident.",
    );
    let evidence = direct_doc(
        "enrich-doc-evidence",
        "MemoryService routes MCP search through the resident.",
    );
    service.upsert(target.clone());
    service.upsert(evidence.clone());
    let reviewed_target = service
        .source_document_by_id(&target.doc_id)
        .unwrap()
        .clone();
    let reviewed_evidence = service
        .source_document_by_id(&evidence.doc_id)
        .unwrap()
        .clone();
    let target_hash = crate::source::source_document_hash(&reviewed_target).unwrap();
    let evidence_hash = crate::source::source_document_hash(&reviewed_evidence).unwrap();
    let request = serde_json::json!({
        "doc_id": target.doc_id,
        "expected_source_hash": target_hash,
        "key_phrases": [{"text":"shared MCP MemoryService search","kind":"thing"}],
        "evidence": [{"doc_id": evidence.doc_id, "expected_source_hash": evidence_hash}]
    });

    let receipt = service.enrich_doc_mcp(&request).unwrap();
    assert_eq!(receipt["status"], "enriched");
    assert_eq!(
        receipt["changed_document_ids"],
        serde_json::json!([target.doc_id])
    );
    let enriched = service.source_document_by_id(&target.doc_id).unwrap();
    assert!(enriched
        .key_phrases
        .iter()
        .any(|phrase| phrase.text == "shared MCP MemoryService search"));
    assert!(service
        .source_document_by_id(&evidence.doc_id)
        .unwrap()
        .key_phrases
        .is_empty());

    let stale = serde_json::json!({
        "doc_id": target.doc_id,
        "expected_source_hash": crate::source::source_document_hash(enriched).unwrap(),
        "key_phrases": [{"text":"another phrase","kind":"thing"}],
        "evidence": [{"doc_id": evidence.doc_id, "expected_source_hash":"stale"}]
    });
    assert!(service
        .enrich_doc_mcp(&stale)
        .unwrap_err()
        .to_string()
        .contains("evidence source hash"));
}

#[test]
fn review_search_pages_then_feedback_labels_search_and_retries_idempotently() {
    let root = search_feedback_temp_dir("feedback-roundtrip");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    service.project_rules = Some(Arc::new(
        crate::reasoning_rules::ProjectReasoningRules::open(&root),
    ));
    let mut document = direct_doc(
        "feedback-search-doc",
        "Weigh the old and new project command interface using a consistent vocabulary.",
    );
    document
        .filters
        .insert("session_id".to_string(), "session-feedback-1".to_string());
    service.upsert(document.clone());

    // Frozen control corpus: only the experimental service will receive
    // the provider's enrichment below.
    let mut control = MemoryService::in_memory(PipelineOptions::default());
    control.upsert(document.clone());
    control.refresh().unwrap();
    let replay_queries = [
        "QZXVProtocol",
        "latest QZXVProtocol",
        "QZXVProtocol version",
    ];
    let recall_at_one = |service: &mut MemoryService| {
        replay_queries
            .iter()
            .filter(|query| {
                service
                    .search_with_filters(query, "user-a", None, 1, &BTreeMap::new(), None)
                    .unwrap()
                    .iter()
                    .any(|hit| hit.doc_id == document.doc_id)
            })
            .count()
    };
    assert_eq!(recall_at_one(&mut service), 0, "baseline Recall@1");
    assert_eq!(recall_at_one(&mut control), 0, "control Recall@1");

    let query = "weigh old and new project command interface QZXVProtocol";
    assert!(service.comparison_operands_for_query(query).is_empty());
    let indexed_document = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    let result = serde_json::json!({
        "results": [{"id": document.doc_id, "source_hash": crate::source::source_document_hash(&indexed_document).unwrap(), "content": document.content}]
    });
    let response = serde_json::json!({
        "result": {"content": [{"text": serde_json::to_string(&result).unwrap()}]}
    });
    crate::telemetry::record_mcp_call(
        &root,
        "claude",
        "search",
        Some(serde_json::json!("request-feedback-1")),
        &serde_json::json!({"query":query, "session_id":"session-feedback-1"}),
        4,
        false,
        response,
    )
    .unwrap();

    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let record = parse_mcp_search_history(&history).unwrap().remove(0).1;
    let search_id = mcp_search_id(&record);
    let page = service
        .review_search_ready(&root, &serde_json::json!({"page_size":1}))
        .unwrap();
    assert_eq!(page["searches"].as_array().unwrap().len(), 1);
    assert_eq!(page["searches"][0]["search_id"], search_id);
    assert_eq!(
        page["searches"][0]["documents"].as_array().unwrap().len(),
        1
    );

    let exact = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    assert_eq!(exact["documents"][0]["doc_id"], document.doc_id);
    let source_hash = exact["documents"][0]["source_hash"].as_str().unwrap();
    assert_eq!(exact["reviewed"], false);
    assert_eq!(exact["search_id"], search_id);
    let document_judgments = serde_json::json!([{
        "doc_id": document.doc_id,
        "relevance": "relevant",
        "feedback": "Contains the project command-interface decision."
    }]);

    for (invalid_judgments, expected_error) in [
        (serde_json::json!([]), "missing document judgment"),
        (
            serde_json::json!([{"doc_id":"not-returned", "relevance":"relevant"}]),
            "missing document judgment",
        ),
        (
            serde_json::json!([
            {"doc_id":document.doc_id, "relevance":"relevant"},
            {"doc_id":document.doc_id, "relevance":"not_relevant"}
            ]),
            "duplicate document judgment",
        ),
    ] {
        let invalid = serde_json::json!({
            "search_id": search_id,
            "label": "partially_helpful",
            "feedback": "Checking the per-document judgment contract.",
            "document_judgments": invalid_judgments
        });
        assert!(service
            .search_feedback_mcp(&root, &invalid)
            .unwrap_err()
            .to_string()
            .contains(expected_error));
    }

    let ungrounded = serde_json::json!({
        "search_id": search_id,
        "label": "partially_helpful",
        "feedback": "Proposed an unrelated phrase.",
        "document_judgments": document_judgments,
        "query_rule_additions": {
            "query_current_state_markers": ["unrelated secret phrase"]
        }
    });
    assert!(service
        .search_feedback_mcp(&root, &ungrounded)
        .unwrap_err()
        .to_string()
        .contains("must appear in the reviewed query or source documents"));

    let invalid_update = serde_json::json!({
        "search_id": search_id,
        "label": "partially_helpful",
        "feedback": "The result was close but needs a supported phrase kind.",
        "document_judgments": document_judgments,
        "updates": [{
            "doc_id": document.doc_id,
            "expected_source_hash": source_hash,
            "key_phrases": [{"text":"project command interface", "kind":"unsupported"}],
            "evidence": [{"doc_id":document.doc_id, "expected_source_hash":source_hash}]
        }]
    });
    assert!(service
        .search_feedback_mcp(&root, &invalid_update)
        .unwrap_err()
        .to_string()
        .contains("unknown search_feedback argument: updates"));
    let state_after_rejection =
        read_search_review_state(&root.join(".membrick/search-review-state.json")).unwrap();
    assert!(state_after_rejection["reviews"].get(&search_id).is_none());

    let unsupported_rule = serde_json::json!({
        "search_id": search_id,
        "label": "partially_helpful",
        "feedback": "Use a supported rule list for this grounded query cue.",
        "document_judgments": document_judgments,
        "query_rule_additions": {
            "query_not_a_supported_list": ["project command interface"]
        }
    });
    assert!(service
        .search_feedback_mcp(&root, &unsupported_rule)
        .unwrap_err()
        .to_string()
        .contains("not a supported query-only rule"));
    let state_after_rule_rejection =
        read_search_review_state(&root.join(".membrick/search-review-state.json")).unwrap();
    assert!(state_after_rule_rejection["reviews"]
        .get(&search_id)
        .is_none());

    let submit = serde_json::json!({
        "search_id": search_id,
        "label": "partially_helpful",
        "feedback": "The result was close but should also match the project's CLI terminology.",
        "document_judgments": document_judgments,
        "query_rule_additions": {
            "query_current_state_markers": ["project command interface"],
            "query_comparison_markers": ["weigh"]
        }
    });
    let receipt = service
        .search_feedback_mcp(&root, &submit)
        .unwrap_or_else(|error| {
            panic!(
                "{error:#}; review document={:?}; submit={submit}",
                exact["documents"][0]
            )
        });
    assert_eq!(receipt["status"], "reviewed");
    assert_eq!(receipt["changed_documents"], 0);
    assert!(receipt["changed_document_ids"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(receipt["accepted_query_rules"], 2);
    assert_eq!(receipt["judged_documents"], 1);
    assert_eq!(receipt["publication_status"], "not_required");
    assert!(service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .key_phrases
        .is_empty());
    assert_eq!(
        recall_at_one(&mut service),
        0,
        "search feedback does not enrich documents"
    );
    assert_eq!(recall_at_one(&mut control), 0, "unchanged control Recall@1");
    assert_eq!(
        service.comparison_operands_for_query(query),
        vec![
            "old project command interface QZXVProtocol".to_string(),
            "new project command interface QZXVProtocol".to_string()
        ],
        "search_feedback should activate the accepted rule in this service immediately"
    );
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    assert_eq!(
        reviewed["prior_feedback"]["document_judgments"][0]["rank"],
        1
    );
    assert_eq!(
        reviewed["prior_feedback"]["document_judgments"][0]["doc_id"],
        document.doc_id
    );
    assert_eq!(
        reviewed["prior_feedback"]["document_judgments"][0]["source_hash"],
        source_hash
    );

    // Simulate a crash after source publication but before receipt
    // persistence. The pending retry must recognize its own update even
    // though the target is also cited evidence and its hash has changed.
    let state_path = root.join(".membrick/search-review-state.json");
    let mut pending = read_search_review_state(&state_path).unwrap();
    pending["reviews"][&search_id]["status"] = serde_json::json!("pending");
    pending["reviews"][&search_id]
        .as_object_mut()
        .unwrap()
        .remove("receipt");
    write_search_review_state(&state_path, &pending).unwrap();
    let retry = service.search_feedback_mcp(&root, &submit).unwrap();
    assert_eq!(retry, receipt);
    let rules_path = root.join(".membrick/rules.json");
    let overlay: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rules_path).unwrap()).unwrap();
    assert_eq!(
        overlay["additions"]["query_current_state_markers"][0],
        "project command interface"
    );
    let changed_retry = serde_json::json!({
        "search_id": search_id,
        "label": "helpful",
        "feedback": "Different review for the same search.",
        "document_judgments": document_judgments
    });
    assert!(service
        .search_feedback_mcp(&root, &changed_retry)
        .unwrap_err()
        .to_string()
        .contains("already has different feedback"));
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    assert_eq!(reviewed["reviewed"], true);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_service_applies_external_query_rule_edit_without_restart() {
    let root = std::env::current_dir()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join("target")
        .join(format!("membrick-rule-service-{}", std::process::id()));
    let state_dir = root.join(".membrick");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&state_dir).unwrap();

    let mut service = MemoryService::in_memory(PipelineOptions::default());
    service.project_rules = Some(Arc::new(
        crate::reasoning_rules::ProjectReasoningRules::open(&root),
    ));
    let query = "weigh old and new project command interface";
    assert!(service.comparison_operands_for_query(query).is_empty());

    std::fs::write(
        state_dir.join("rules.json"),
        r#"{"schema_version":1,"additions":{"query_comparison_markers":["weigh"]}}"#,
    )
    .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let operands = service.comparison_operands_for_query(query);
        if !operands.is_empty() {
            assert_eq!(
                operands,
                vec![
                    "old project command interface".to_string(),
                    "new project command interface".to_string()
                ]
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "workspace service did not activate an external query-rule edit"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_search_returns_only_documents_from_the_search_result() {
    let root = search_feedback_temp_dir("review-session-scope");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let mut earlier = direct_doc(
        "review-session-earlier-observation",
        "The first observation records the original deployment constraint.",
    );
    earlier
        .filters
        .insert("session_id".into(), "session-review-history".into());
    let mut result_hit = direct_doc(
        "review-session-result-hit",
        "The later observation records the verified deployment fix.",
    );
    result_hit
        .filters
        .insert("session_id".into(), "session-review-history".into());
    let mut other_session = direct_doc(
        "review-session-other-session",
        "A different session records unrelated deployment work.",
    );
    other_session
        .filters
        .insert("session_id".into(), "session-review-other".into());
    service.upsert(earlier.clone());
    service.upsert(result_hit.clone());
    service.upsert(other_session.clone());

    let result = serde_json::json!({
        "results": [{"id": result_hit.doc_id, "content": result_hit.content}]
    });
    let response = serde_json::json!({
        "result": {"content": [{"type":"text", "text":serde_json::to_string(&result).unwrap()}]}
    });
    crate::telemetry::record_mcp_call(
            &root,
            "claude",
            "search",
            Some(serde_json::json!("request-explicit-session-review")),
            &serde_json::json!({"query":"verified deployment fix", "session_id":"session-review-history"}),
            2,
            false,
            response.clone(),
        )
        .unwrap();
    crate::telemetry::record_mcp_call(
        &root,
        "codex",
        "search",
        Some(serde_json::json!("request-unattributed-session-review")),
        &serde_json::json!({"query":"verified deployment fix"}),
        2,
        false,
        response,
    )
    .unwrap();

    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let entries = parse_mcp_search_history(&history).unwrap();
    let explicit_id = entries
        .iter()
        .find(|(_, record)| record.session_attribution == "explicit")
        .map(|(_, record)| mcp_search_id(record))
        .unwrap();
    let absent_id = entries
        .iter()
        .find(|(_, record)| record.session_attribution == "absent")
        .map(|(_, record)| mcp_search_id(record))
        .unwrap();

    let explicit = service
        .review_search_ready(&root, &serde_json::json!({"search_id":explicit_id}))
        .unwrap();
    let explicit_document_ids = explicit["documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["doc_id"].as_str())
        .collect::<HashSet<_>>();
    assert_eq!(
        explicit_document_ids,
        HashSet::from([result_hit.doc_id.as_str()])
    );
    assert!(!explicit_document_ids.contains(earlier.doc_id.as_str()));
    assert!(!explicit_document_ids.contains(other_session.doc_id.as_str()));

    let absent = service
        .review_search_ready(&root, &serde_json::json!({"search_id":absent_id}))
        .unwrap();
    let absent_document_ids = absent["documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["doc_id"].as_str())
        .collect::<HashSet<_>>();
    assert_eq!(
        absent_document_ids,
        HashSet::from([result_hit.doc_id.as_str()])
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_search_lookup_skips_oversized_unrelated_history_records() {
    let root = search_feedback_temp_dir("review-oversized-history");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    crate::telemetry::record_mcp_call(
        &root,
        "codex",
        "search",
        Some(serde_json::json!("oversized-history-search")),
        &serde_json::json!({"query":"find the bounded history record"}),
        1,
        false,
        serde_json::json!({"result":{"content":[{"text":"{\\\"results\\\":[]}"}]}}),
    )
    .unwrap();
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let search_line = std::fs::read(&history_path).unwrap();
    let search_id = parse_mcp_search_history(&search_line)
        .unwrap()
        .pop()
        .unwrap()
        .1
        .trace_id
        .unwrap();
    let mut history = vec![b'x'; 2048];
    history.push(b'\n');
    history.extend(search_line);
    std::fs::write(&history_path, history).unwrap();

    let record = find_mcp_search_record_with_limit(&history_path, &search_id, 1024).unwrap();
    assert_eq!(mcp_search_id(&record), search_id);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_search_reports_indexing_while_history_rebuild_is_incomplete() {
    let root = search_feedback_temp_dir("review-index-pending");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("index-pending-search")),
            &serde_json::json!({"query":"pending while lookup index builds"}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::json!({"results":[]}).to_string()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let search_id = mcp_search_id(&parse_mcp_search_history(&history).unwrap()[0].1);
    let state = service.search_history_index.state_for(&root);
    state.lock().unwrap().worker_running = true;

    let response = service
        .review_search_mcp(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();

    assert_eq!(response["status"], "indexing");
    assert_eq!(response["search_id"], search_id);
    assert!(response["search"].is_null());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_search_keeps_partial_history_tail_pending() {
    let root = search_feedback_temp_dir("review-partial-history-tail");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    crate::telemetry::record_mcp_call(
        &root,
        "codex",
        "search",
        Some(serde_json::json!("partial-tail-search")),
        &serde_json::json!({"query":"partially appended search"}),
        1,
        false,
        serde_json::json!({"result":{"content":[{"text":"{\"results\":[]}"}]}}),
    )
    .unwrap();
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let history = std::fs::read(&history_path).unwrap();
    let search_id = mcp_search_id(&parse_mcp_search_history(&history).unwrap()[0].1);
    std::fs::write(&history_path, &history[..history.len() - 1]).unwrap();
    let state = service.search_history_index.state_for(&root);
    {
        let mut state = state.lock().unwrap();
        let metadata = std::fs::metadata(&history_path).unwrap();
        state.generation = Some(search_history_generation(&metadata));
        state.initialized = true;
        state.scanned_offset = metadata.len();
        state.captured_end = metadata.len();
        state.partial_line = history[..history.len() - 1].to_vec();
        state.worker_running = true;
    }

    let response = service
        .review_search_mcp(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();

    assert_eq!(response["status"], "indexing");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_search_absence_stays_pending_after_history_replacement() {
    let root = search_feedback_temp_dir("review-replaced-history");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    crate::telemetry::record_mcp_call(
        &root,
        "codex",
        "search",
        Some(serde_json::json!("replaced-history-search")),
        &serde_json::json!({"query":"original history"}),
        1,
        false,
        serde_json::json!({"result":{"content":[{"text":"{\"results\":[]}"}]}}),
    )
    .unwrap();
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let original = std::fs::read(&history_path).unwrap();
    let state = service.search_history_index.state_for(&root);
    {
        let mut state = state.lock().unwrap();
        let metadata = std::fs::metadata(&history_path).unwrap();
        state.generation = Some(search_history_generation(&metadata));
        state.initialized = true;
        state.scanned_offset = metadata.len();
        state.captured_end = metadata.len();
        state.worker_running = true;
    }

    let replacement_path = root.join(".membrick/replacement.jsonl");
    let replacement = original
        .iter()
        .map(|byte| if *byte == b'o' { b'x' } else { *byte })
        .collect::<Vec<_>>();
    assert_eq!(replacement.len(), original.len());
    std::fs::write(&replacement_path, replacement).unwrap();
    std::fs::rename(&replacement_path, &history_path).unwrap();

    let response = service
        .review_search_mcp(
            &root,
            &serde_json::json!({"search_id":"not-in-replacement"}),
        )
        .unwrap();

    assert_eq!(response["status"], "indexing");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn paginated_review_streams_past_oversized_history_records() {
    let root = search_feedback_temp_dir("review-streaming-page");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("streaming-page-search")),
            &serde_json::json!({"query":"bounded page fixture"}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::json!({"results":[]}).to_string()}]}}),
        )
        .unwrap();
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let search_line = std::fs::read(&history_path).unwrap();
    let mut history = vec![b'x'; MAX_REVIEW_HISTORY_RECORD_BYTES + 1];
    history.push(b'\n');
    history.extend(search_line);
    std::fs::write(&history_path, history).unwrap();

    let response = service
        .review_search_ready(&root, &serde_json::json!({"page_size":1}))
        .unwrap();
    assert_eq!(response["oversized_records_skipped"], 1);
    assert_eq!(response["searches"].as_array().unwrap().len(), 1);
    assert_eq!(response["has_more"], false);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_search_history_rows_are_reported_and_make_absence_uncertain() {
    let root = search_feedback_temp_dir("review-malformed-history-row");
    std::fs::create_dir_all(&root).unwrap();
    for request_id in ["before-corruption", "after-corruption"] {
        crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!(request_id)),
            &serde_json::json!({"query":format!("query {request_id}")}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":"{\"results\":[]}"}]}}),
        )
        .unwrap();
    }
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let original = std::fs::read(&history_path).unwrap();
    let lines = original
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    let mut corrupted = lines[0].to_vec();
    corrupted.extend_from_slice(b"{not-json}\n");
    corrupted.extend_from_slice(lines[1]);
    std::fs::write(&history_path, corrupted).unwrap();

    let service = MemoryService::in_memory(PipelineOptions::default());
    let page = service
        .review_search_ready(&root, &serde_json::json!({"page_size":10}))
        .unwrap();
    assert_eq!(page["searches"].as_array().unwrap().len(), 2);
    assert_eq!(page["malformed_records_skipped"], 1);
    assert_eq!(page["first_malformed_offset"], lines[0].len());

    let exact = service
        .review_search_mcp(&root, &serde_json::json!({"search_id":"never-recorded"}))
        .unwrap();
    assert_eq!(exact["status"], "history_incomplete");
    assert_eq!(exact["malformed_records_skipped"], 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_search_ids_do_not_block_later_review_page_entries() {
    let root = search_feedback_temp_dir("review-duplicate-search-id");
    std::fs::create_dir_all(&root).unwrap();
    for request_id in ["duplicated-search", "later-search"] {
        crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!(request_id)),
            &serde_json::json!({"query":format!("query {request_id}")}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":"{\"results\":[]}"}]}}),
        )
        .unwrap();
    }
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let original = std::fs::read(&history_path).unwrap();
    let lines = original
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    let later_search_id = mcp_search_id(&parse_mcp_search_history(&original).unwrap()[1].1);
    let mut duplicated = lines[0].to_vec();
    duplicated.extend_from_slice(lines[0]);
    duplicated.extend_from_slice(lines[1]);
    std::fs::write(&history_path, duplicated).unwrap();

    let service = MemoryService::in_memory(PipelineOptions::default());
    let page = service
        .review_search_ready(&root, &serde_json::json!({"page_size":10}))
        .unwrap();
    assert_eq!(page["searches"].as_array().unwrap().len(), 1);
    assert_eq!(page["searches"][0]["search_id"], later_search_id);
    assert_eq!(page["ambiguous_searches_skipped"], 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_search_review_rejects_responses_over_the_output_budget() {
    let root = search_feedback_temp_dir("review-output-budget");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    let result = serde_json::json!({
        "results": [{"id":"large-result", "content":"x".repeat(MAX_REVIEW_PAGE_RESPONSE_BYTES + 128 * 1024)}]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("large-search-result")),
            &serde_json::json!({"query":"large result"}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let search_id = mcp_search_id(&parse_mcp_search_history(&history).unwrap()[0].1);

    let error = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("exceeds the response byte limit"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_feedback_can_ground_terms_beyond_the_document_preview() {
    let root = search_feedback_temp_dir("feedback-full-document-grounding");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    service.project_rules = Some(Arc::new(
        crate::reasoning_rules::ProjectReasoningRules::open(&root),
    ));
    let content = format!(
        "{} trailing-grounding-needle",
        "ordinary report text ".repeat(600)
    );
    let document = direct_doc("feedback-long-document", &content);
    service.upsert(document.clone());
    let indexed = service.source_document_by_id(&document.doc_id).unwrap();
    let result = serde_json::json!({
        "results": [{
            "id": document.doc_id,
            "source_hash": crate::source::source_document_hash(indexed).unwrap(),
            "content": "ordinary report text"
        }]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("long-document-grounding-search")),
            &serde_json::json!({"query":"ordinary report text"}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let search_id = mcp_search_id(&parse_mcp_search_history(&history).unwrap()[0].1);
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    assert_eq!(reviewed["documents"][0]["content_truncated"], true);

    let feedback = serde_json::json!({
        "search_id": search_id,
        "label": "helpful",
        "feedback": "The term is supported by the complete source document.",
        "document_judgments": [{"doc_id":document.doc_id, "relevance":"relevant"}],
        "query_rule_additions": {
            "query_explanation_terms": ["trailing-grounding-needle"]
        }
    });
    let receipt = service.search_feedback_mcp(&root, &feedback).unwrap();

    assert_eq!(receipt["status"], "reviewed");
    assert_eq!(receipt["accepted_query_rules"], 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_exact_search_reviews_share_the_index_safely() {
    let root = search_feedback_temp_dir("review-concurrent-index");
    std::fs::create_dir_all(root.join(".membrick")).unwrap();
    let service = Arc::new(MemoryService::in_memory(PipelineOptions::default()));
    crate::telemetry::record_mcp_call(
        &root,
        "codex",
        "search",
        Some(serde_json::json!("concurrent-review-search")),
        &serde_json::json!({"query":"concurrent lookup"}),
        1,
        false,
        serde_json::json!({"result":{"content":[{"text":"{\"results\":[]}"}]}}),
    )
    .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let search_id = mcp_search_id(&parse_mcp_search_history(&history).unwrap()[0].1);

    std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| {
                let service = Arc::clone(&service);
                let root = root.clone();
                let search_id = search_id.clone();
                scope.spawn(move || {
                    service
                        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap()["search_id"], search_id);
        }
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_search_bounds_each_returned_source_document_preview() {
    let root = search_feedback_temp_dir("review-source-preview");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let large_content = format!("Relevant source text. {}", "x".repeat(24 * 1024));
    let document = direct_doc("review-large-hit", &large_content);
    service.upsert(document.clone());
    let result = serde_json::json!({
        "results": [{"id": document.doc_id, "content":"Relevant source text."}]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("large-source-preview")),
            &serde_json::json!({"query":"Relevant source text"}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let search_id = parse_mcp_search_history(&history)
        .unwrap()
        .pop()
        .unwrap()
        .1
        .trace_id
        .unwrap();

    let response = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    let returned_document = &response["documents"][0];
    let excerpt = returned_document["document"]["content"].as_str().unwrap();
    assert!(excerpt.len() <= MAX_REVIEW_DOCUMENT_CONTENT_BYTES);
    assert_eq!(returned_document["content_truncated"], true);
    assert_eq!(returned_document["content_bytes"], document.content.len());
    assert_eq!(
        returned_document["source_hash"],
        crate::source::source_document_hash(
            &service
                .store
                .source_document_by_id(&document.doc_id)
                .unwrap(),
        )
        .unwrap()
    );
    assert!(serde_json::to_vec(&response).unwrap().len() < 20_000);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn enrich_doc_publishes_scoped_document_supersession() {
    let root = search_feedback_temp_dir("feedback-supersession");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let mut old = direct_doc("feedback-old-decision", "The old deployment decision.");
    old.filters
        .insert("semantic_scope".into(), "project:feedback".into());
    let mut new = direct_doc(
        "feedback-new-decision",
        "The corrected deployment decision.",
    );
    new.filters
        .insert("semantic_scope".into(), "project:feedback".into());
    service.upsert(old.clone());
    service.upsert(new.clone());
    let result = serde_json::json!({
        "results": [
            {"id": old.doc_id, "content": old.content},
            {"id": new.doc_id, "content": new.content}
        ]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "claude",
            "search",
            Some(serde_json::json!("request-feedback-supersession")),
            &serde_json::json!({"query":"corrected deployment decision"}),
            4,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let record = parse_mcp_search_history(&history).unwrap().remove(0).1;
    let search_id = mcp_search_id(&record);
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    let hashes = reviewed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|document| {
            (
                document["doc_id"].as_str().unwrap().to_string(),
                document["source_hash"].as_str().unwrap().to_string(),
            )
        })
        .collect::<HashMap<_, _>>();
    let submit = serde_json::json!({
        "doc_id": new.doc_id,
        "expected_source_hash": hashes[&new.doc_id],
        "supersedes_document_id": old.doc_id,
        "evidence": [{"doc_id": old.doc_id, "expected_source_hash": hashes[&old.doc_id]}]
    });
    let receipt = service.enrich_doc_mcp(&submit).unwrap();
    assert_eq!(receipt["publication_status"], "published");
    assert_eq!(
        receipt["adjudication"]["decisions"][0]["kind"],
        "supersedes"
    );
    assert_eq!(
        receipt["adjudication"]["retired_doc_ids"],
        serde_json::json!([old.doc_id])
    );
    let old_state = service.semantic_document_state(&old.doc_id);
    assert_eq!(
        old_state.status,
        Some(crate::semantic_relations::SemanticStatus::Superseded)
    );
    assert_eq!(
        old_state.superseded_by.as_deref(),
        Some(new.doc_id.as_str())
    );
    assert_eq!(
        service
            .source_document_by_id(&new.doc_id)
            .unwrap()
            .filters
            .get("supersedes_id"),
        Some(&old.doc_id)
    );
    let retry = service.enrich_doc_mcp(&submit).unwrap();
    assert_eq!(retry["status"], "unchanged");
    assert_eq!(
        retry["adjudication"]["retired_doc_ids"],
        serde_json::json!([old.doc_id])
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn enrich_doc_publishes_workspace_document_supersession() {
    let root = search_feedback_temp_dir("feedback-workspace-supersession");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::retain_workspace_store(
        MemoryService::in_memory(PipelineOptions::default()),
        MemoryService::in_memory(PipelineOptions::default()),
    );
    let mut old = direct_doc("feedback-workspace-old", "The old workspace decision.");
    old.filters
        .insert("semantic_scope".into(), "project:feedback-workspace".into());
    let mut new = direct_doc(
        "feedback-workspace-new",
        "The corrected workspace decision.",
    );
    new.filters
        .insert("semantic_scope".into(), "project:feedback-workspace".into());
    service
        .memory_store_mut(MemoryStore::Workspace)
        .upsert(old.clone());
    service
        .memory_store_mut(MemoryStore::Workspace)
        .upsert(new.clone());
    let result = serde_json::json!({
        "results": [
            {"id": old.doc_id, "content": old.content},
            {"id": new.doc_id, "content": new.content}
        ]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "claude",
            "search",
            Some(serde_json::json!("request-feedback-workspace-supersession")),
            &serde_json::json!({"query":"corrected workspace decision"}),
            4,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let record = parse_mcp_search_history(&history).unwrap().remove(0).1;
    let search_id = mcp_search_id(&record);
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    let hashes = reviewed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|document| {
            (
                document["doc_id"].as_str().unwrap().to_string(),
                document["source_hash"].as_str().unwrap().to_string(),
            )
        })
        .collect::<HashMap<_, _>>();
    let submit = serde_json::json!({
        "doc_id": new.doc_id,
        "expected_source_hash": hashes[&new.doc_id],
        "supersedes_document_id": old.doc_id,
        "evidence": [{"doc_id": old.doc_id, "expected_source_hash": hashes[&old.doc_id]}]
    });
    let receipt = service.enrich_doc_mcp(&submit).unwrap();
    assert_eq!(receipt["publication_status"], "published");
    assert_eq!(
        receipt["adjudication"]["decisions"][0]["kind"],
        "supersedes"
    );
    assert_eq!(
        receipt["adjudication"]["retired_doc_ids"],
        serde_json::json!([old.doc_id])
    );
    let workspace = service.memory_store(MemoryStore::Workspace);
    assert_eq!(
        workspace.semantic_document_state(&old.doc_id).status,
        Some(crate::semantic_relations::SemanticStatus::Superseded)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn enrich_doc_rejects_cross_scope_document_supersession() {
    let root = search_feedback_temp_dir("feedback-cross-scope");
    std::fs::create_dir_all(&root).unwrap();
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let mut old = direct_doc("feedback-other-scope", "A decision in another project.");
    old.filters
        .insert("semantic_scope".into(), "project:other".into());
    let mut new = direct_doc("feedback-this-scope", "A proposed replacement decision.");
    new.filters
        .insert("semantic_scope".into(), "project:this".into());
    service.upsert(old.clone());
    service.upsert(new.clone());
    let result = serde_json::json!({
        "results": [
            {"id": old.doc_id, "content": old.content},
            {"id": new.doc_id, "content": new.content}
        ]
    });
    crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(serde_json::json!("request-feedback-cross-scope")),
            &serde_json::json!({"query":"replacement decision"}),
            4,
            false,
            serde_json::json!({"result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}}),
        )
        .unwrap();
    let history = std::fs::read(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
    let record = parse_mcp_search_history(&history).unwrap().remove(0).1;
    let search_id = mcp_search_id(&record);
    let reviewed = service
        .review_search_ready(&root, &serde_json::json!({"search_id":search_id}))
        .unwrap();
    let hashes = reviewed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|document| {
            (
                document["doc_id"].as_str().unwrap().to_string(),
                document["source_hash"].as_str().unwrap().to_string(),
            )
        })
        .collect::<HashMap<_, _>>();
    let submit = serde_json::json!({
        "doc_id": new.doc_id,
        "expected_source_hash": hashes[&new.doc_id],
        "supersedes_document_id": old.doc_id,
        "evidence": [{"doc_id": old.doc_id, "expected_source_hash": hashes[&old.doc_id]}]
    });
    let error = service.enrich_doc_mcp(&submit).unwrap_err();
    assert!(error
        .to_string()
        .contains("outside the target's semantic scope"));
    let state = read_search_review_state(&root.join(".membrick/search-review-state.json")).unwrap();
    assert!(state["reviews"].get(&search_id).is_none());
    assert_eq!(
        service
            .source_document_by_id(&new.doc_id)
            .unwrap()
            .filters
            .get("supersedes_id"),
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_document_supersession_cannot_cross_backing_stores() {
    let mut service = MemoryService::retain_workspace_store(
        MemoryService::in_memory(PipelineOptions::default()),
        MemoryService::in_memory(PipelineOptions::default()),
    );
    let mut captured = direct_doc("feedback-captured-target", "Captured decision.");
    captured
        .filters
        .insert("semantic_scope".into(), "project:shared".into());
    let mut workspace = direct_doc("feedback-workspace-source", "Workspace correction.");
    workspace
        .filters
        .insert("semantic_scope".into(), "project:shared".into());
    service.upsert(captured.clone());
    service
        .memory_store_mut(MemoryStore::Workspace)
        .upsert(workspace.clone());
    let mut proposal = enrichment(&workspace, "workspace correction");
    proposal.supersedes_document_id = Some(captured.doc_id);

    let error = service
        .prepare_source_document_enrichments(vec![proposal], Some("cross-store"))
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("must be in the same backing store"));
    assert!(service
        .memory_store(MemoryStore::Workspace)
        .source_document_by_id(&workspace.doc_id)
        .unwrap()
        .filters
        .get("supersedes_id")
        .is_none());
}

#[test]
fn review_search_cursor_pages_and_rejects_a_replaced_history_file() {
    let root = search_feedback_temp_dir("review-pagination");
    std::fs::create_dir_all(&root).unwrap();
    let service = MemoryService::in_memory(PipelineOptions::default());
    for index in 0..2 {
        crate::telemetry::record_mcp_call(
            &root,
            "claude",
            "search",
            Some(serde_json::json!(format!("pagination-{index}"))),
            &serde_json::json!({"query":format!("pagination query {index}")}),
            1,
            false,
            serde_json::json!({"result":{"content":[{"text":"{}"}]}}),
        )
        .unwrap();
    }

    let first_page = service
        .review_search_ready(&root, &serde_json::json!({"page_size":1}))
        .unwrap();
    assert_eq!(first_page["searches"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["has_more"], true);
    let cursor = first_page["next_cursor"].as_str().unwrap().to_string();
    assert!(cursor.starts_with("v2:"));

    let second_page = service
        .review_search_ready(&root, &serde_json::json!({"page_size":1,"cursor":cursor}))
        .unwrap();
    assert_eq!(second_page["searches"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["has_more"], false);
    assert_ne!(
        first_page["searches"][0]["search_id"],
        second_page["searches"][0]["search_id"]
    );

    let first_line = std::fs::read_to_string(root.join(".membrick/mcp-call-history.jsonl"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let history_path = root.join(".membrick/mcp-call-history.jsonl");
    let mut changed_history = std::fs::read_to_string(&history_path).unwrap();
    changed_history = changed_history.replacen("pagination query 0", "replaced query 0", 1);
    assert_ne!(first_line, changed_history.lines().next().unwrap());
    let replacement_path = history_path.with_extension("replacement");
    std::fs::write(&replacement_path, changed_history).unwrap();
    std::fs::rename(&replacement_path, &history_path).unwrap();
    assert!(service
        .review_search_ready(&root, &serde_json::json!({"page_size":1,"cursor":cursor}))
        .unwrap_err()
        .to_string()
        .contains("search history changed"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_history_cursor_parses_generations_containing_colons() {
    let (generation, offset, snapshot_end) =
        parse_search_history_cursor_fields("v2:SystemTime { tv_sec: 123, tv_nsec: 456 }:17:901")
            .unwrap();

    assert_eq!(generation, "SystemTime { tv_sec: 123, tv_nsec: 456 }");
    assert_eq!(offset, 17);
    assert_eq!(snapshot_end, 901);
}

#[test]
fn published_key_phrase_enrichment_changes_search_matching() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let document = direct_doc(
        "enrich-search-target",
        "The project interface is described here.",
    );
    service.upsert(document.clone());
    let reviewed = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    service
        .enrich_source_document(enrichment(&reviewed, "QZXVProtocol"))
        .unwrap();
    service.refresh_index().unwrap();

    let record = service.record_by_id(&document.doc_id).unwrap();
    assert!(record
        .key_entities
        .iter()
        .any(|entity| entity.text.eq_ignore_ascii_case("QZXVProtocol")));
    let direct = service.store.query("QZXVProtocol", 5).unwrap();
    assert!(
        direct.iter().any(|hit| hit.doc_id == document.doc_id),
        "raw index did not retrieve the enriched key entity: {}",
        serde_json::to_string(&direct).unwrap()
    );
    let result = service
        .search_with_filters(
            "QZXVProtocol",
            "enrichment-test",
            None,
            5,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(
        result.iter().any(|hit| hit.doc_id == document.doc_id),
        "enriched entity was not retrievable: {}",
        serde_json::to_string(&result).unwrap()
    );
}

#[test]
fn extractor_refresh_preserves_provider_enrichment_on_the_source_document() {
    let mut service = MemoryService::in_memory(PipelineOptions::default());
    let document = direct_doc("enrich-refresh", "A project-specific command interface.");
    service.upsert(document.clone());
    let reviewed = service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .clone();
    service
        .enrich_source_document(enrichment(&reviewed, "project command interface"))
        .unwrap();

    let content_hash = crate::pipeline::key_phrase_content_hash(&document.content);
    service.store.set_key_phrases(
        &document.doc_id,
        &content_hash,
        vec![crate::source::KeyPhrase {
            text: "command interface".to_string(),
            kind: "thing".to_string(),
            origin: Some(crate::source::KeyPhraseOrigin::Extractor),
        }],
    );

    let phrases = &service
        .source_document_by_id(&document.doc_id)
        .unwrap()
        .key_phrases;
    assert!(phrases.iter().any(|phrase| {
        phrase.text == "command interface"
            && phrase.origin == Some(crate::source::KeyPhraseOrigin::Extractor)
    }));
    assert!(phrases.iter().any(|phrase| {
        phrase.text == "project command interface"
            && phrase.origin == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
    }));
}

#[test]
fn key_phrase_backfill_applies_synchronously_without_worker() {
    let script = write_canary_extractor("backfill-sync");
    let mut service = extractor_service(&script);
    // store.upsert bypasses the background queue entirely, the way a
    // hook-written document looks after its process died.
    service.store.upsert(direct_doc(
        "sync-doc",
        "the canary phrase in the quick brown fox story",
    ));
    service.store.refresh().unwrap();
    assert!(service.key_phrase_backfill_needed());
    assert_eq!(service.backfill_key_phrases(), 1);
    assert_eq!(
        key_phrases_of(&service, "sync-doc"),
        vec!["canary phrase".to_string()]
    );
    assert!(!service.key_phrase_backfill_needed());
    // Idempotent: a second backfill finds nothing to do.
    assert_eq!(service.backfill_key_phrases(), 0);
}

#[test]
fn key_phrase_backfill_runs_inside_search_with_filters() {
    // The MCP query entry point triggers the backfill itself, so a
    // hook-written document's phrases are extracted before this query
    // runs: phrases are absent before the search and present after.
    let script = write_canary_extractor("backfill-search");
    let mut service = extractor_service(&script);
    service.store.upsert(direct_doc(
        "search-doc",
        "the canary phrase in the quick brown fox story",
    ));
    service.store.refresh().unwrap();
    assert!(key_phrases_of(&service, "search-doc").is_empty());
    let results = service
        .search_with_filters("canary phrase", "test", None, 10, &BTreeMap::new(), None)
        .unwrap();
    assert!(
        results.iter().any(|r| r.doc_id == "search-doc"),
        "query should retrieve the backfilled document"
    );
    assert_eq!(
        key_phrases_of(&service, "search-doc"),
        vec!["canary phrase".to_string()]
    );
    // The rebuilt record carries the phrase as a key entity, so the
    // query that triggered the backfill already sees it indexed.
    // Head-noun admission: the key entity is "phrase", not the whole
    // "canary phrase" (modifiers are not the thing denoted).
    let record = service.store.record_by_id("search-doc").unwrap();
    assert!(record.key_entities.iter().any(|e| e.text == "phrase"));
}

#[test]
fn key_phrase_empty_extraction_is_durably_complete() {
    // An extractor that runs clean but finds nothing still stamps the
    // document: it must not be re-extracted on every later query.
    let script = write_empty_extractor("backfill-empty");
    let mut service = extractor_service(&script);
    service
        .store
        .upsert(direct_doc("empty-doc", "some ordinary content here"));
    assert_eq!(service.backfill_key_phrases(), 1);
    assert!(key_phrases_of(&service, "empty-doc").is_empty());
    assert!(!service.key_phrase_backfill_needed());
    assert_eq!(service.backfill_key_phrases(), 0);
}

#[test]
fn key_phrase_backfill_persists_across_restart_with_stamp() {
    let script = write_canary_extractor("backfill-restart");
    let dir = canonical_temp_dir("backfill-restart");
    std::fs::create_dir_all(&dir).unwrap();
    let options = PipelineOptions {
        key_phrase_enrichment: true,
        // Test exercises the spaCy extractor; opt out of Python-free.
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(script),
        ..PipelineOptions::default()
    };
    {
        let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
        service
            .store
            .upsert(direct_doc("restart-doc", "persistent content for restart"));
        service.store.refresh().unwrap();
        assert_eq!(service.backfill_key_phrases(), 1);
        // refresh persists; dropping here closes the "hook process".
    }
    let reopened = MemoryService::at_path(&dir, options).unwrap();
    assert_eq!(
        key_phrases_of(&reopened, "restart-doc"),
        vec!["canary phrase".to_string()]
    );
    // The extraction stamp survived the restart: no re-extraction.
    assert!(!reopened.key_phrase_backfill_needed());
}

#[test]
fn key_phrase_timeout_stays_retryable() {
    // A timed-out extractor run must not stamp the document as done.
    let script = write_fake_extractor("backfill-slow", true);
    let probe = direct_doc("slow-doc", "some slow content here");
    let turns = crate::segments::relations::relation_turns_from_docs(&[&probe]);
    assert!(
        run_key_phrase_extraction_bounded(&turns, Some(&script), 0).is_none(),
        "zero-second timeout must report a transient failure"
    );

    let mut service = extractor_service(&script);
    service
        .store
        .upsert(direct_doc("slow-doc", "some slow content here"));
    let docs = vec![direct_doc("slow-doc", "some slow content here")];
    // None = transient failure: nothing applied, nothing stamped, so a
    // later query retries the document.
    assert_eq!(service.apply_key_phrase_backfill(&docs, None), 0);
    assert!(key_phrases_of(&service, "slow-doc").is_empty());
    assert!(service.key_phrase_backfill_needed());
}

#[test]
fn key_phrase_backfill_drops_handled_docs_from_worker_queue() {
    let script = write_canary_extractor("backfill-drop");
    let mut service = extractor_service(&script);
    // MemoryService::upsert queues the turn for the background worker.
    service.upsert(direct_doc("drop-doc", "content for queue drop"));
    assert_eq!(service.enrichment_queue_len(), 1);
    assert_eq!(service.backfill_key_phrases(), 1);
    assert_eq!(service.enrichment_queue_len(), 0);
    assert!(!service.key_phrase_backfill_needed());
}

#[test]
fn key_phrase_backfill_rejects_stale_results_after_replace() {
    let script = write_canary_extractor("backfill-stale");
    let mut service = extractor_service(&script);
    service
        .store
        .upsert(direct_doc("stale-doc", "version one content here"));
    assert_eq!(service.backfill_key_phrases(), 1);
    assert!(!service.key_phrase_backfill_needed());
    // Same id, new content: the old stamp no longer matches, so the
    // document needs extraction again.
    service
        .store
        .upsert(direct_doc("stale-doc", "version two completely different"));
    assert!(service.key_phrase_backfill_needed());
    assert_eq!(service.backfill_key_phrases(), 1);
    assert_eq!(
        key_phrases_of(&service, "stale-doc"),
        vec!["canary phrase".to_string()]
    );
    assert!(!service.key_phrase_backfill_needed());
}

#[test]
fn key_phrase_refresh_index_drains_enrichment_inbox() {
    let script = write_fake_extractor("refresh-drain", false);
    let mut service = enrichment_service(&script);
    add_message(
        &mut service,
        "req-drain",
        "user-a",
        "sess-a",
        "I love the Paris jazz festival",
    );
    let doc_id = doc_id_for("user-a", "req-drain");
    // No follow-up write: the worker finishes on its own, and
    // refresh_index applies whatever the inbox holds.
    let applied = poll_until(std::time::Duration::from_secs(20), || {
        let _ = service.refresh_index();
        !key_phrases_of(&service, &doc_id).is_empty()
    });
    assert!(applied, "refresh_index should drain the enrichment inbox");
}

#[test]
fn key_phrase_worker_drains_more_than_one_batch() {
    let script = write_fake_extractor("worker-multibatch", false);
    let mut service = enrichment_service(&script);
    for i in 0..40 {
        add_message(
            &mut service,
            &format!("req-batch-{i}"),
            "user-a",
            "sess-a",
            &format!("message number {i} about topology"),
        );
    }
    // 40 documents exceed one 32-document worker batch; the worker must
    // keep draining without another write to wake it.
    let applied = poll_until(std::time::Duration::from_secs(60), || {
        let _ = service.refresh_index();
        (0..40).all(|i| {
            !key_phrases_of(&service, &doc_id_for("user-a", &format!("req-batch-{i}"))).is_empty()
        })
    });
    assert!(
        applied,
        "worker should drain all batches without another write"
    );
}

#[test]
fn key_phrase_backfill_fail_open_without_extractor() {
    let mut service = MemoryService::in_memory(PipelineOptions {
        key_phrase_enrichment: true,
        // Test the spaCy fail-open path; opt out of Python-free default.
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(std::path::PathBuf::from("/nonexistent/extractor.py")),
        ..PipelineOptions::default()
    });
    service
        .store
        .upsert(direct_doc("fail-doc", "content that cannot be enriched"));
    // Broken script: the subprocess cannot run, so the document is NOT
    // stamped — it stays queued for a later retry — and the service
    // keeps working: fail-open at the query level.
    assert_eq!(service.backfill_key_phrases(), 0);
    assert!(key_phrases_of(&service, "fail-doc").is_empty());
    assert!(service.key_phrase_backfill_needed());
    let response = service
        .search(SearchRequest {
            query: "enriched".into(),
            options: None,
            user_id: "user-a".into(),
            top_k: 5,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    let _ = response;
}

/// Mirror of a provider hook's write sequence: a short-lived process
/// opens the service with the hook's options, upserts, refreshes, and
/// exits before the background worker finishes. The follow-up query
/// (a fresh process, like the MCP server) must still get phrases.
fn hook_write_then_query(name: &str, provider: &str, doc_id: &str, content: &str, query: &str) {
    let script = write_canary_extractor(name);
    let dir = canonical_temp_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    // Same option shape as the Claude Code / Codex / Gemini hooks:
    // segmented layout with top-3 routing, enrichment on.
    // These tests exercise the spaCy extractor; opt out of the
    // Python-free default explicitly.
    let options = PipelineOptions {
        memory_index_layout: crate::MemoryIndexLayout::Segmented {
            query_top_n: 3,
            routing_strategy: crate::segments::SegmentRoutingStrategy::LocalDistinctiveness,
        },
        key_phrase_enrichment: true,
        ner_provider: Tier1NerProvider::Spacy,
        extractor_script: Some(script),
        ..PipelineOptions::default()
    };
    {
        let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
        let mut doc = direct_doc(doc_id, content);
        doc.author_agent = Some(provider.to_string());
        service.upsert(doc);
        service.refresh_index().unwrap();
        // Hook process exits here; its worker never finishes.
    }
    let mut query_side = MemoryService::at_path(&dir, options).unwrap();
    // The hook's worker died with its process: nothing extracted yet.
    assert!(
        key_phrases_of(&query_side, doc_id).is_empty(),
        "{provider} hook-written doc should start without phrases"
    );
    let results = query_side
        .search_with_filters(query, "test", None, 10, &BTreeMap::new(), None)
        .unwrap();
    assert!(
        results.iter().any(|r| r.doc_id == doc_id),
        "{provider} hook-written doc should be retrievable"
    );
    assert_eq!(
        key_phrases_of(&query_side, doc_id),
        vec!["canary phrase".to_string()],
        "{provider} hook-written doc should carry backfilled phrases"
    );
}

#[test]
fn key_phrase_backfill_covers_claude_code_hook_path() {
    hook_write_then_query(
        "hook-claude",
        "claude-code",
        "claude-hook-doc",
        "claude code session outcome notes with the canary phrase about the deployment",
        "canary phrase",
    );
}

#[test]
fn key_phrase_backfill_covers_codex_hook_path() {
    hook_write_then_query(
        "hook-codex",
        "codex",
        "codex-hook-doc",
        "codex session transcript with the canary phrase about the refactor",
        "canary phrase",
    );
}

#[test]
fn key_phrase_backfill_covers_gemini_hook_path() {
    hook_write_then_query(
        "hook-gemini",
        "gemini-cli",
        "gemini-hook-doc",
        "gemini cli tool call record with the canary phrase for the migration",
        "canary phrase",
    );
}
