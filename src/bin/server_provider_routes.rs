use super::*;

pub(super) fn router() -> Router<AppState> {
    let router = Router::new();
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "openclaw",
        feature = "hermes",
        feature = "roo-runtime"
    ))]
    let router = router
        .route(
            "/provider-memory/add/batch",
            post(provider_memory_add_batch),
        )
        .route("/provider-memory/search", post(provider_memory_search))
        .route(MCP_TOOLS_CALL_PATH, post(provider_mcp_tool_call))
        .route(
            MCP_DOCUMENTS_UPSERT_PATH,
            post(provider_mcp_document_upsert),
        );
    #[cfg(any(feature = "claude-code", feature = "codex"))]
    let router = router
        .route(
            "/provider-memory/captures",
            post(provider_transcript_capture),
        )
        .route("/api/captures", get(api_transcript_captures));
    #[cfg(feature = "openclaw")]
    let router = router.route("/integrations/openclaw/hooks/:kind", post(openclaw_hook));
    router
}

pub(super) async fn openclaw_hook(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let Some(kind) = crate::integrations::openclaw::hooks::OpenClawHookKind::from_str(&kind) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail":"unknown OpenClaw hook"})),
        )
            .into_response();
    };
    // These hooks read/write workspace-wide provider data, not tenant data.
    if state.tenant_id.is_some() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"workspace hooks are unavailable in tenant mode"})),
        )
            .into_response();
    }
    let mut requested_workspace = None;
    for pointer in [
        "/ctx/workspaceDir",
        "/event/context/workspaceDir",
        "/event/workspaceDir",
    ] {
        if let Some(candidate) = payload
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            let canonical = match std::fs::canonicalize(candidate.trim()) {
                Ok(path) if path.is_dir() => path,
                _ => return StatusCode::FORBIDDEN.into_response(),
            };
            if requested_workspace
                .as_ref()
                .is_some_and(|previous| previous != &canonical)
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"detail":"hook workspace fields do not match"})),
                )
                    .into_response();
            }
            requested_workspace = Some(canonical);
        }
    }
    let requested_workspace = requested_workspace
        .unwrap_or_else(|| state.project_root.clone())
        .to_string_lossy()
        .into_owned();
    let runtime = match state.workspace_registry.resolve(&requested_workspace).await {
        Ok(runtime) => runtime,
        Err(error) => return error.into_response(),
    };
    let root = runtime.project_root.clone();
    // Serialize admission per workspace. The MemoryService writer remains the
    // owner of persistence and publication for the captured documents.
    let Ok(writer) = runtime.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let provider_writer = runtime.staged_writer.clone();
    let result = match tokio::task::spawn_blocking(move || {
        // Keep admission held if the HTTP timeout cancels the awaiting task.
        let _writer = writer;
        crate::integrations::openclaw::hooks::process_http_hook_with_writer(
            kind,
            payload,
            &root,
            |document| provider_writer.upsert_documents_blocking(vec![document]),
        )
    })
    .await
    {
        Ok(result) => result,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    (StatusCode::OK, Json(result)).into_response()
}

pub(super) async fn provider_memory_add_batch(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(requests): Json<Vec<AddRequest>>,
) -> impl IntoResponse {
    if requests.is_empty() || requests.len() > 128 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"batch must contain between 1 and 128 requests"})),
        )
            .into_response();
    }
    for request in &requests {
        let value = match serde_json::to_value(request) {
            Ok(value) => value,
            Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        };
        if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
            return error.into_response();
        }
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    drop(writer);
    let Some(worker) = &state.staged_writer else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match worker.add(requests, true).await {
        Ok(response) => Json(response).into_response(),
        Err(status) => status.into_response(),
    }
}

pub(super) async fn provider_memory_search(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    let requested_workspace = value
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let mut request = match serde_json::from_value::<SearchRequest>(value) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid request"})),
            )
                .into_response()
        }
    };
    if let Some(auth) = auth.as_ref() {
        request.scope = Some(auth.0.user_id.clone());
    } else if let Some(tenant) = state.tenant_id.as_deref() {
        request.scope = Some(tenant.to_string());
    } else {
        // Server-token provider adapters share one workspace store across
        // integrations. Keep conversation state scoped to the caller's
        // configured identity, while allowing retrieval across provider-owned
        // memories and ordinary workspace documents. JWT callers remain
        // isolated to their authenticated user below.
        let caller_scope = request.user_id.clone();
        request.user_id.clear();
        if request.scope.as_deref().is_none_or(str::is_empty) {
            request.scope = Some(caller_scope);
        }
    }
    let published_view = if let Some(workspace) = requested_workspace.as_deref() {
        match state.workspace_registry.resolve(workspace).await {
            Ok(runtime) => runtime.staged_writer.read_view(),
            Err(error) => return error.into_response(),
        }
    } else {
        state.provider_published_view()
    };
    let started = Instant::now();
    let result = published_view.and_then(|view| view.search_cached(request));
    state.telemetry.record_query(
        started.elapsed().as_millis() as u64,
        result.is_err(),
        result
            .as_ref()
            .map(|response| response.data.is_empty())
            .unwrap_or(false),
    );
    match result {
        Ok(response) => Json(serde_json::to_value(response).unwrap()).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) async fn provider_transcript_capture(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> axum::response::Response {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let runtime = match state
        .workspace_registry
        .resolve(
            value
                .get("workspace")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .await
    {
        Ok(runtime) => runtime,
        Err(error) => return error.into_response(),
    };
    let request = match serde_json::from_value::<
        crate::memory_api::transcript_capture::CaptureRequest,
    >(value)
    {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response()
        }
    };
    if let Err(error) = request.validate(&runtime.project_root) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":error.to_string()})),
        )
            .into_response();
    }
    let worker = runtime.transcript_captures.clone();
    match tokio::task::spawn_blocking(move || worker.submit(request)).await {
        Ok(Ok(status)) => (
            StatusCode::ACCEPTED,
            Json(json!({"accepted":true, "capture":status})),
        )
            .into_response(),
        Ok(Err(error)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail":error.to_string()})),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) async fn api_transcript_captures(
    State(state): State<AppState>,
) -> axum::response::Response {
    let runtime = match state
        .workspace_registry
        .resolve(&state.project_root.to_string_lossy())
        .await
    {
        Ok(runtime) => runtime,
        Err(error) => return error.into_response(),
    };
    match runtime.transcript_captures.status() {
        Ok(captures) => Json(json!({"captures":captures})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":error.to_string()})),
        )
            .into_response(),
    }
}

pub(super) async fn provider_mcp_document_upsert(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> axum::response::Response {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let workspace = value
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let runtime = match state.workspace_registry.resolve(workspace).await {
        Ok(runtime) => runtime,
        Err(error) => return error.into_response(),
    };
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !crate::integrations::mcp_tools::provider_filter_values().contains(&provider) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"invalid provider"})),
        )
            .into_response();
    }
    let documents_value = value
        .get("documents")
        .cloned()
        .or_else(|| value.get("document").map(|document| json!([document])));
    let Some(documents_value) = documents_value else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"documents are required"})),
        )
            .into_response();
    };
    let documents = match serde_json::from_value::<Vec<crate::SourceDocument>>(documents_value) {
        Ok(documents) if !documents.is_empty() => documents,
        _ => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid documents"})),
            )
                .into_response()
        }
    };
    for document in &documents {
        if document.filters.get("provider").map(String::as_str) != Some(provider) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"document provider does not match request context"})),
            )
                .into_response();
        }
        if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
            if document.filters.get("session_id").map(String::as_str) != Some(session_id) {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"detail":"document session does not match request context"})),
                )
                    .into_response();
            }
        }
    }
    let writer = &runtime.staged_writer;
    let audit = server_writer::CaptureAudit {
        provider: provider.to_string(),
        user_id: value
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or("mcp")
            .to_string(),
        session_id: value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    };
    let result = if let Some(group) = value.get("replace_group") {
        let key = group.get("key").and_then(Value::as_str).unwrap_or_default();
        let group_value = group
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if key.is_empty() || group_value.is_empty() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"replace_group requires key and value"})),
            )
                .into_response();
        }
        let audit_ids = documents
            .iter()
            .map(|document| document.doc_id.clone())
            .collect::<Vec<_>>();
        match writer
            .replace_group(key.to_string(), group_value.to_string(), documents)
            .await
        {
            Ok(()) => writer
                .mutate(move |service| {
                    service.record_capture_mutation(
                        &audit.provider,
                        &audit.user_id,
                        &audit.session_id,
                        &audit_ids,
                    )?;
                    Ok(Value::Null)
                })
                .await
                .map(|_| ())
                .map_err(|error| {
                    server_writer::mutation_status(&error, StatusCode::INTERNAL_SERVER_ERROR)
                }),
            Err(status) => Err(status),
        }
    } else {
        writer.upsert_documents(documents, Some(audit)).await
    };
    match result {
        Ok(()) => Json(json!({"success": true, "durable": true})).into_response(),
        Err(status) => (
            status,
            Json(json!({"detail":"provider memory write failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn provider_mcp_tool_call(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let workspace = value
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let runtime = match state.workspace_registry.resolve(workspace).await {
        Ok(runtime) => runtime,
        Err(error) => return error.into_response(),
    };
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !crate::integrations::mcp_tools::provider_filter_values().contains(&provider) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"invalid provider"})),
        )
            .into_response();
    }
    let Some(tool) = value
        .get("tool")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"tool is required"})),
        )
            .into_response();
    };
    let arguments = value.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"arguments must be an object"})),
        )
            .into_response();
    }
    let workspace = runtime.project_root.to_string_lossy().into_owned();
    let result: anyhow::Result<Value> = async {
        // Derive trusted invocation metadata only from the resident's active
        // provider session. Request JSON, including top-level transport fields
        // and extension tool arguments, is caller-controlled.
        let invocation_session_id = runtime
            .staged_writer
            .read_view()?
            .current_session_id(provider);
        match tool.as_str() {
            "search" => {
                if arguments.as_object().is_some_and(|args| {
                    args.keys().any(|key| {
                        !["query", "top_k", "provider", "session_id"].contains(&key.as_str())
                    })
                }) {
                    anyhow::bail!("unknown search argument");
                }
                let query = arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                anyhow::ensure!(!query.is_empty(), "query is required");
                let top_k = arguments
                    .get("top_k")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .clamp(1, 20) as usize;
                let filters = crate::integrations::mcp_tools::search_provider_filters(&arguments)
                    .map_err(anyhow::Error::msg)?;
                let view = runtime.staged_writer.read_view()?;
                let session_id = crate::integrations::mcp_tools::resolve_search_session_id(
                    &arguments, &view, provider,
                )
                .map_err(anyhow::Error::msg)?;
                let attribution = if arguments
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    "explicit"
                } else if session_id.is_some() {
                    "inferred"
                } else {
                    "absent"
                };
                let mut payload = view.search_agent_cached(
                    query,
                    provider,
                    session_id.as_deref(),
                    top_k,
                    &filters,
                )?;
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "__membrick_telemetry".to_string(),
                        json!({
                            "session_id": session_id,
                            "session_attribution": attribution,
                        }),
                    );
                }
                Ok(payload)
            }
            "recall_search" => {
                if arguments.as_object().is_some_and(|args| {
                    args.keys()
                        .any(|key| !["query", "result_count", "source"].contains(&key.as_str()))
                }) {
                    anyhow::bail!("unknown recall_search argument");
                }
                let query = arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let result_count = arguments
                    .get("result_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 100) as usize;
                let source = arguments
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("all");
                let view = runtime.staged_writer.read_view()?;
                let response = crate::integrations::recall::recall_from_service(
                    &workspace,
                    query,
                    result_count,
                    source,
                    view.as_ref(),
                )?;
                Ok(serde_json::to_value(response)?)
            }
            "review_search" => {
                let root = runtime.project_root.clone();
                runtime
                    .staged_writer
                    .read_view()?
                    .review_search_mcp(&root, &arguments)
            }
            "search_feedback" => {
                let root = runtime.project_root.clone();
                let arguments = arguments.clone();
                runtime
                    .staged_writer
                    .mutate(move |service| service.search_feedback_mcp(&root, &arguments))
                    .await
            }
            "enrich_doc" => {
                let arguments = arguments.clone();
                runtime
                    .staged_writer
                    .mutate(move |service| service.enrich_doc_mcp(&arguments))
                    .await
            }
            "add_memory" => {
                let mut write_arguments = arguments.clone();
                if write_arguments.get("session_id").is_none() {
                    if let Some(session_id) = invocation_session_id.as_deref() {
                        write_arguments["session_id"] = json!(session_id);
                    }
                }
                let request = {
                    let owner = runtime
                        .service
                        .read()
                        .map_err(|_| anyhow::anyhow!("provider memory service poisoned"))?;
                    crate::integrations::mcp_tools::build_add_memory_request(
                        &write_arguments,
                        &owner,
                        provider,
                    )
                    .map_err(anyhow::Error::msg)?
                };
                let writer = &runtime.staged_writer;
                let value = writer
                    .add(vec![request], true)
                    .await
                    .map_err(|status| anyhow::anyhow!("provider write failed with {status}"))?;
                Ok(value
                    .as_array()
                    .and_then(|values| values.first())
                    .cloned()
                    .unwrap_or(value))
            }
            "get_memory" => {
                let mut owner = runtime
                    .service
                    .write()
                    .map_err(|_| anyhow::anyhow!("provider memory service poisoned"))?;
                crate::integrations::mcp_tools::dispatch_memory_tool(
                    &tool, &arguments, &mut owner, provider,
                )
                .map_err(anyhow::Error::msg)
            }
            "delete_memory" => {
                let mut owner = runtime
                    .service
                    .write()
                    .map_err(|_| anyhow::anyhow!("provider memory service poisoned"))?;
                crate::integrations::mcp_tools::dispatch_memory_tool(
                    &tool, &arguments, &mut owner, provider,
                )
                .map_err(anyhow::Error::msg)
            }
            "list_memories" => {
                if arguments
                    .as_object()
                    .is_some_and(|args| args.keys().any(|key| key != "limit"))
                {
                    anyhow::bail!("unknown list_memories argument");
                }
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 100) as usize;
                let view = runtime.staged_writer.read_view()?;
                Ok(view.list_memories_payload(limit))
            }
            "info" => {
                anyhow::ensure!(
                    arguments.as_object().is_none_or(|args| args.is_empty()),
                    "info takes no arguments"
                );
                let view = runtime.staged_writer.read_view()?;
                Ok(json!({
                    "root": workspace,
                    "docs_count": view.docs_count(),
                    "reasoning_rules": view.reasoning_rules_status(),
                    "transcript_captures": transcript_capture_status(&runtime),
                }))
            }
            name if crate::integrations::mcp_tools::is_extension_tool_name(name)
                && crate::integrations::mcp_tools::extension_tool_is_mutating(name) =>
            {
                let writer = &runtime.staged_writer;
                let provider = provider.to_string();
                let write_tool = tool.clone();
                let invocation_session_id = invocation_session_id.clone();
                let payload = writer
                    .mutate(move |owner| {
                        crate::integrations::mcp_tools::dispatch_extension_tool_with_session(
                            &write_tool,
                            &arguments,
                            owner,
                            "mcp",
                            &workspace,
                            &provider,
                            invocation_session_id.as_deref(),
                        )
                        .map_err(anyhow::Error::msg)
                    })
                    .await?;
                // The mutation is applied to the owner before `mutate`
                // returns, so dashboard reads already see this change.
                if let Some(event) = change_event(&tool, &payload) {
                    writer.changes().publish(event);
                }
                Ok(payload)
            }
            name if crate::integrations::mcp_tools::is_extension_tool_name(name) => {
                let mut owner = runtime
                    .service
                    .write()
                    .map_err(|_| anyhow::anyhow!("provider memory service poisoned"))?;
                crate::integrations::mcp_tools::dispatch_extension_tool_with_session(
                    &tool,
                    &arguments,
                    &mut owner,
                    "mcp",
                    &workspace,
                    provider,
                    invocation_session_id.as_deref(),
                )
                .map_err(anyhow::Error::msg)
            }
            _ => anyhow::bail!("unknown memory tool: {tool}"),
        }
    }
    .await;
    match result {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::UNPROCESSABLE_ENTITY),
            Json(json!({"detail":format!("{error:#}")})),
        )
            .into_response(),
    }
}
