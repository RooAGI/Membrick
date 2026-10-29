use super::*;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard))
        .route("/dashboard/app.js", get(dashboard_app))
        .route("/dashboard/event-detail.js", get(dashboard_event_detail))
        .route(
            "/dashboard/capture-progress.mjs",
            get(dashboard_capture_progress),
        )
        .route(
            "/dashboard/search-evaluation.mjs",
            get(dashboard_search_evaluation),
        )
        .route("/dashboard/board-ui.mjs", get(dashboard_board_ui))
        .route("/dashboard/styles.css", get(dashboard_styles))
        .route("/dashboard/rooagi-logo.png", get(dashboard_logo))
        .route("/api/status", get(api_status))
        .route("/api/timeseries", get(api_timeseries))
        .route("/api/search-reviews", get(api_search_reviews))
        .route("/api/integrations", get(api_integrations))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/:session_key/events", get(api_session_events))
        .route("/api/events", get(api_events))
        .route("/api/metrics", get(api_metrics))
}

pub(super) async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../../dashboard/index.html"))
}

pub(super) async fn dashboard_app() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../../dashboard/app.js"),
    )
}

pub(super) async fn dashboard_event_detail() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../../dashboard/event-detail.js"),
    )
}

pub(super) async fn dashboard_search_evaluation() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../../dashboard/search-evaluation.mjs"),
    )
}

pub(super) async fn dashboard_board_ui() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../../dashboard/board-ui.mjs"),
    )
}

pub(super) async fn dashboard_capture_progress() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../../dashboard/capture-progress.mjs"),
    )
}

pub(super) async fn dashboard_styles() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../dashboard/styles.css"),
    )
}

pub(super) async fn dashboard_logo() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static [u8],
) {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../../dashboard/rooagi-logo.png"),
    )
}

pub(super) async fn api_status(State(state): State<AppState>) -> impl IntoResponse {
    let inspection = match state.service.read() {
        Ok(service) => service.memory_store(MemoryStore::Workspace).inspection(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":"status unavailable"})),
            )
                .into_response()
        }
    };
    let telemetry = project_query_snapshot(&state.project_root)
        .ok()
        .flatten()
        .unwrap_or_else(|| state.telemetry.snapshot());
    Json(DashboardStatus {
        status: if inspection.dirty {
            "degraded"
        } else {
            "healthy"
        },
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: now_unix_ms()
            .saturating_sub(telemetry.started_at_ms)
            .checked_div(1_000)
            .unwrap_or_default(),
        index: dashboard_index_status(inspection),
        telemetry,
        integrations: dashboard_integrations(&state.project_root),
        transcript_captures: {
            let registry = state.workspace_registry.state.lock().await;
            registry
                .entries
                .get(&state.project_root)
                .map(|runtime| transcript_capture_status(runtime))
                .unwrap_or_else(|| json!([]))
        },
    })
    .into_response()
}

pub(super) async fn api_timeseries(State(state): State<AppState>) -> impl IntoResponse {
    Json(
        project_query_snapshot(&state.project_root)
            .ok()
            .flatten()
            .unwrap_or_else(|| state.telemetry.snapshot()),
    )
}

pub(super) async fn api_search_reviews(
    State(state): State<AppState>,
    Query(query): Query<DashboardSearchReviewsQuery>,
) -> impl IntoResponse {
    match crate::memory_api::MemoryService::dashboard_search_feedback_reviews(
        &state.project_root,
        query.offset.unwrap_or_default(),
        query.page_size.unwrap_or(25),
    ) {
        Ok(reviews) => Json(reviews).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": format!("search reviews unavailable: {error}")})),
        )
            .into_response(),
    }
}

pub(super) async fn api_integrations(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "integrations": dashboard_integrations(&state.project_root),
        "note": "Provider hooks and MCP processes report independently; this server does not infer agent connectivity from index health."
    }))
}

pub(super) async fn api_sessions(State(state): State<AppState>) -> impl IntoResponse {
    let mut sessions = BTreeMap::<(String, String), Vec<ProviderLifecycleEvent>>::new();
    for integration in dashboard_integrations(&state.project_root) {
        for event in integration.events {
            sessions
                .entry((event.provider.clone(), event.session_key.clone()))
                .or_default()
                .push(event);
        }
    }
    let sessions = sessions
        .into_iter()
        .map(|((provider, session_key), mut events)| {
            events.sort_by_key(|event| event.timestamp_ms);
            let first_seen_ms = events
                .first()
                .map(|event| event.timestamp_ms)
                .unwrap_or_default();
            let last = events.last();
            DashboardSession {
                provider,
                session_key,
                event_count: events.len(),
                first_seen_ms,
                last_seen_ms: last.map(|event| event.timestamp_ms).unwrap_or_default(),
                last_event: last.map(|event| event.event.clone()).unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    Json(json!({"sessions": sessions}))
}

pub(super) async fn api_session_events(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> impl IntoResponse {
    let events = recent_provider_events(&state.project_root)
        .into_iter()
        .filter(|event| event.session_key == session_key)
        .collect::<Vec<_>>();
    Json(json!({"session_key": session_key, "events": events}))
}

pub(super) async fn api_events(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({"events": recent_provider_events(&state.project_root)}))
}

pub(super) async fn api_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let integrations = dashboard_integrations(&state.project_root);
    let query = project_query_snapshot(&state.project_root)
        .ok()
        .flatten()
        .unwrap_or_else(|| state.telemetry.snapshot())
        .query_summary;
    Json(json!({
        "query": query,
        "providers": integrations.into_iter().map(|item| json!({
            "provider": item.provider,
            "compiled": item.compiled,
            "state": item.state,
            "events_total": item.events_total,
            "sessions_started": item.sessions_started,
            "sessions_ended": item.sessions_ended,
            "sessions_active": item.sessions_active,
            "retrieval_events": item.retrieval_events,
            "capture_events": item.capture_events,
        })).collect::<Vec<_>>(),
    }))
}
