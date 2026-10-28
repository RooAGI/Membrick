use super::*;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root_redirect))
        .route("/health", get(health).post(health))
        .route("/metrics", get(metrics))
}

pub(super) async fn root_redirect() -> Redirect {
    Redirect::temporary("/dashboard")
}

pub(super) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let canonical = state
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| state.project_root.clone());
    let digest = sha2::Sha256::digest(canonical.to_string_lossy().as_bytes());
    let workspace_id: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "workspace_id": workspace_id
    }))
}

pub(super) async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let query = project_query_snapshot(&state.project_root)
        .ok()
        .flatten()
        .unwrap_or_else(|| state.telemetry.snapshot())
        .query_summary;
    let integrations = dashboard_integrations(&state.project_root);
    let mut body = String::from("# Membrick operational metrics\n");
    body.push_str("# HELP membrick_query_requests_window Search requests in the retained telemetry window.\n# TYPE membrick_query_requests_window gauge\n");
    body.push_str(&format!(
        "membrick_query_requests_window {}\n",
        query.requests
    ));
    body.push_str("# HELP membrick_query_errors_window Search errors in the retained telemetry window.\n# TYPE membrick_query_errors_window gauge\n");
    body.push_str(&format!("membrick_query_errors_window {}\n", query.errors));
    body.push_str("# HELP membrick_query_empty_results_window Searches with no results in the retained telemetry window.\n# TYPE membrick_query_empty_results_window gauge\n");
    body.push_str(&format!(
        "membrick_query_empty_results_window {}\n",
        query.empty_results
    ));
    body.push_str("# HELP membrick_query_requests_per_second Search request rate over the retained telemetry window.\n# TYPE membrick_query_requests_per_second gauge\n");
    body.push_str(&format!(
        "membrick_query_requests_per_second {}\n",
        query.requests_per_second
    ));
    body.push_str("# HELP membrick_query_error_rate Fraction of searches that returned errors in the retained telemetry window.\n# TYPE membrick_query_error_rate gauge\n");
    body.push_str(&format!("membrick_query_error_rate {}\n", query.error_rate));
    body.push_str("# HELP membrick_query_empty_result_rate Fraction of searches with no results in the retained telemetry window.\n# TYPE membrick_query_empty_result_rate gauge\n");
    body.push_str(&format!(
        "membrick_query_empty_result_rate {}\n",
        query.empty_result_rate
    ));
    body.push_str("# HELP membrick_query_latency_ms Estimated query latency quantile in milliseconds over the retained telemetry window.\n# TYPE membrick_query_latency_ms gauge\n");
    body.push_str(&format!(
        "membrick_query_latency_ms{{quantile=\"0.5\"}} {}\n",
        query.p50_ms
    ));
    body.push_str(&format!(
        "membrick_query_latency_ms{{quantile=\"0.95\"}} {}\n",
        query.p95_ms
    ));
    body.push_str("# HELP membrick_provider_compiled Whether this server build includes the provider integration.\n# TYPE membrick_provider_compiled gauge\n");
    body.push_str("# HELP membrick_provider_observed Whether Membrick has ever received lifecycle telemetry for the provider.\n# TYPE membrick_provider_observed gauge\n");
    body.push_str("# HELP membrick_provider_events_total Lifecycle events recorded for the provider.\n# TYPE membrick_provider_events_total counter\n");
    body.push_str("# HELP membrick_provider_sessions_started_total Provider sessions started.\n# TYPE membrick_provider_sessions_started_total counter\n");
    body.push_str("# HELP membrick_provider_sessions_ended_total Provider sessions ended.\n# TYPE membrick_provider_sessions_ended_total counter\n");
    body.push_str("# HELP membrick_provider_retrieval_events_total Provider lifecycle events categorized as retrieval.\n# TYPE membrick_provider_retrieval_events_total counter\n");
    body.push_str("# HELP membrick_provider_capture_events_total Provider lifecycle events categorized as capture.\n# TYPE membrick_provider_capture_events_total counter\n");
    body.push_str("# HELP membrick_provider_sessions_active Active provider sessions observed by Membrick.\n# TYPE membrick_provider_sessions_active gauge\n");
    body.push_str("# HELP membrick_provider_last_seen_timestamp_seconds Unix timestamp of the last received provider event, or zero if none.\n# TYPE membrick_provider_last_seen_timestamp_seconds gauge\n");
    body.push_str("# HELP membrick_provider_recent_events Events by bounded lifecycle category in the retained provider event ledger.\n# TYPE membrick_provider_recent_events gauge\n");
    body.push_str("# HELP membrick_provider_token_usage_recent Tokens reported in the retained provider event ledger.\n# TYPE membrick_provider_token_usage_recent gauge\n");
    for (name, help) in [
        ("prompts", "Prompt submissions"),
        ("stops", "Turn stops"),
        ("tool_calls", "Tool invocations"),
        ("tool_failures", "Failed tool results"),
        ("subagent_starts", "Subagent starts"),
        (
            "cycles_completed",
            "Prompts followed by a stop in the same session",
        ),
    ] {
        body.push_str(&format!(
            "# HELP membrick_provider_{name}_total {help} counted since the provider ledger began counting.\n# TYPE membrick_provider_{name}_total counter\n"
        ));
    }
    body.push_str("# HELP membrick_provider_token_usage_total Tokens reported since the provider ledger began counting.\n# TYPE membrick_provider_token_usage_total counter\n");
    for integration in integrations {
        body.push_str(&format!(
            "membrick_provider_events_total{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.events_total
        ));
        body.push_str(&format!(
            "membrick_provider_compiled{{provider=\"{}\"}} {}\n",
            integration.recording_provider,
            u8::from(integration.compiled)
        ));
        body.push_str(&format!(
            "membrick_provider_observed{{provider=\"{}\"}} {}\n",
            integration.recording_provider,
            u8::from(integration.last_seen_ms.is_some())
        ));
        body.push_str(&format!(
            "membrick_provider_sessions_started_total{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.sessions_started
        ));
        body.push_str(&format!(
            "membrick_provider_sessions_ended_total{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.sessions_ended
        ));
        body.push_str(&format!(
            "membrick_provider_retrieval_events_total{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.retrieval_events
        ));
        body.push_str(&format!(
            "membrick_provider_capture_events_total{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.capture_events
        ));
        body.push_str(&format!(
            "membrick_provider_sessions_active{{provider=\"{}\"}} {}\n",
            integration.recording_provider, integration.sessions_active
        ));
        body.push_str(&format!(
            "membrick_provider_last_seen_timestamp_seconds{{provider=\"{}\"}} {}\n",
            integration.recording_provider,
            integration.last_seen_ms.unwrap_or_default() as f64 / 1_000.0
        ));
        if let Some(counters) = &integration.counters {
            for (name, value) in [
                ("prompts", counters.prompts),
                ("stops", counters.stops),
                ("tool_calls", counters.tool_calls),
                ("tool_failures", counters.tool_failures),
                ("subagent_starts", counters.subagent_starts),
                ("cycles_completed", counters.cycles_completed),
            ] {
                body.push_str(&format!(
                    "membrick_provider_{name}_total{{provider=\"{}\"}} {value}\n",
                    integration.recording_provider
                ));
            }
            for (kind, value) in [
                ("input", counters.input_tokens),
                ("output", counters.output_tokens),
                ("cache_creation_input", counters.cache_creation_input_tokens),
                ("cache_read_input", counters.cache_read_input_tokens),
            ] {
                body.push_str(&format!(
                    "membrick_provider_token_usage_total{{provider=\"{}\",kind=\"{kind}\"}} {value}\n",
                    integration.recording_provider
                ));
            }
        }

        let mut recent_categories = BTreeMap::<&str, u64>::new();
        let mut recent_tokens = BTreeMap::<&str, u64>::new();
        for event in &integration.events {
            if matches!(
                event.category.as_str(),
                "session" | "retrieval" | "compaction" | "capture" | "lifecycle"
            ) {
                *recent_categories
                    .entry(event.category.as_str())
                    .or_default() += 1;
            }
            for (kind, value) in [
                ("input", event.input_tokens),
                ("output", event.output_tokens),
                ("cache_creation_input", event.cache_creation_input_tokens),
                ("cache_read_input", event.cache_read_input_tokens),
            ] {
                if let Some(value) = value {
                    let total = recent_tokens.entry(kind).or_default();
                    *total = total.saturating_add(value);
                }
            }
        }
        for category in ["session", "retrieval", "compaction", "capture", "lifecycle"] {
            body.push_str(&format!(
                "membrick_provider_recent_events{{provider=\"{}\",category=\"{}\"}} {}\n",
                integration.recording_provider,
                category,
                recent_categories.get(category).copied().unwrap_or_default()
            ));
        }
        for kind in [
            "input",
            "output",
            "cache_creation_input",
            "cache_read_input",
        ] {
            body.push_str(&format!(
                "membrick_provider_token_usage_recent{{provider=\"{}\",kind=\"{}\"}} {}\n",
                integration.recording_provider,
                kind,
                recent_tokens.get(kind).copied().unwrap_or_default()
            ));
        }
    }

    body.push_str("# HELP membrick_index_source_documents Source documents in the project memory index.\n# TYPE membrick_index_source_documents gauge\n");
    body.push_str("# HELP membrick_index_records Records in the project memory index.\n# TYPE membrick_index_records gauge\n");
    body.push_str("# HELP membrick_index_dirty Whether the memory index has unpublished changes.\n# TYPE membrick_index_dirty gauge\n");
    body.push_str("# HELP membrick_index_store_revision Current mutable store revision.\n# TYPE membrick_index_store_revision gauge\n");
    body.push_str("# HELP membrick_index_snapshot_revision Published search snapshot revision.\n# TYPE membrick_index_snapshot_revision gauge\n");
    body.push_str("# HELP membrick_index_revision_lag Difference between store and published snapshot revisions.\n# TYPE membrick_index_revision_lag gauge\n");
    body.push_str("# HELP membrick_index_segments Number of segments in the published search snapshot.\n# TYPE membrick_index_segments gauge\n");
    if let Ok(service) = state.service.read() {
        let inspection = service.memory_store(MemoryStore::Workspace).inspection();
        body.push_str(&format!(
            "membrick_index_source_documents {}\nmembrick_index_records {}\nmembrick_index_dirty {}\nmembrick_index_store_revision {}\nmembrick_index_snapshot_revision {}\nmembrick_index_revision_lag {}\nmembrick_index_segments {}\n",
            inspection.source_document_count,
            inspection.record_count,
            u8::from(inspection.dirty),
            inspection.store_revision,
            inspection.snapshot_revision,
            inspection.store_revision.saturating_sub(inspection.snapshot_revision),
            inspection.snapshot.map(|snapshot| snapshot.segment_count).unwrap_or_default()
        ));
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}
