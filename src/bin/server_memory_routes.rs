use super::*;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct RestAddRequest {
    #[serde(default)]
    request_id: Option<String>,
    messages: Vec<crate::memory_api::Message>,
    user_id: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, Value>,
}

impl RestAddRequest {
    fn into_service(self) -> crate::memory_api::AddRequestContext {
        let explicit_request_id = self.request_id.is_some();
        let session_id_was_omitted = self.session_id.is_none();
        let request_id = self
            .request_id
            .unwrap_or_else(crate::memory_client::generated_id);
        let session_id = self.session_id.unwrap_or_else(|| {
            if explicit_request_id {
                crate::memory_client::retry_session_id(&self.user_id, &request_id)
            } else {
                crate::memory_client::generated_id()
            }
        });
        crate::memory_api::AddRequestContext {
            request: AddRequestWithMetadata {
                request: AddRequest {
                    request_id,
                    messages: self.messages,
                    user_id: self.user_id,
                    session_id,
                },
                metadata: self.metadata,
            },
            session_id_was_omitted: explicit_request_id && session_id_was_omitted,
        }
    }
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/add", post(add))
        .route("/add/batch", post(add_batch))
        .route("/flush", post(refresh_memories))
        .route("/search", post(search))
        .route("/delete", post(delete))
        .route("/supersede", post(supersede))
        .route("/expire", post(expire))
        .route("/v1/memories", get(list_memories).post(add))
        .route(
            "/v1/memories/:memory_id",
            get(get_memory).patch(update_memory).delete(delete_memory),
        )
        .route("/v1/memories/search", post(search))
        .route("/v1/memories/refresh", post(refresh_memories))
}

pub(super) async fn add(
    State(state): State<AppState>,
    Query(options): Query<WriteOptions>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let request = match serde_json::from_value::<RestAddRequest>(value) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid request"})),
            )
                .into_response()
        }
    }
    .into_service();
    if let Some(worker) = &state.staged_writer {
        return match worker
            .add_with_metadata(request, options.wait_for_visibility)
            .await
        {
            Ok(mut responses) => Json(responses.as_array_mut().unwrap().remove(0)).into_response(),
            Err(status) => (
                status,
                Json(json!({"detail":"mutation failed or writer queue full"})),
            )
                .into_response(),
        };
    }
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

pub(super) async fn add_batch(
    State(state): State<AppState>,
    Query(options): Query<WriteOptions>,
    auth: Option<Extension<AuthContext>>,
    Json(requests): Json<Vec<RestAddRequest>>,
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
    let requests = requests
        .into_iter()
        .map(RestAddRequest::into_service)
        .collect::<Vec<_>>();
    if let Some(worker) = &state.staged_writer {
        return match worker
            .add_with_metadata_batch(requests, options.wait_for_visibility)
            .await
        {
            Ok(responses) => Json(responses).into_response(),
            Err(status) => (
                status,
                Json(json!({"detail":"mutation failed or writer queue full"})),
            )
                .into_response(),
        };
    }
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

pub(super) async fn search(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let mut request = match serde_json::from_value::<SearchRequestWithFilterAst>(value) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid request"})),
            )
                .into_response()
        }
    };
    if request
        .filter_ast
        .as_ref()
        .is_some_and(|ast| ast.validate_mem0_fields().is_err())
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"unsupported or invalid filter expression"})),
        )
            .into_response();
    }
    if let Some(auth) = auth.as_ref() {
        // The JWT subject owns both the documents and conversation context.
        // Ignore caller-provided scope so one user cannot mutate another
        // user's follow-up state.
        request.request.scope = Some(auth.0.user_id.clone());
    }
    // Search uses the already-published immutable index snapshot. Its small
    // mutable side state is internally synchronized, so requests can share
    // the service read lock and run retrieval concurrently.
    //
    // Query-time key-phrase backfill: documents written by provider hooks
    // (separate short-lived processes) never see this process's background
    // enrichment worker, so the first query that needs their phrases
    // extracts them synchronously here. The read-lock check is cheap, so
    // the steady state costs nothing; the extractor (the slow part) runs
    // with no service lock held, and only the fast apply step takes the
    // write lock. A concurrent write landing mid-backfill is safe:
    // application re-validates content hashes and skips stale results.
    // Bounded and fail-open.
    let backfill_needed = if let Some(worker) = &state.staged_writer {
        worker
            .read_view()
            .map(|view| view.key_phrase_backfill_needed())
            .unwrap_or(false)
    } else {
        state
            .service
            .read()
            .map(|service| {
                service
                    .memory_store(MemoryStore::Workspace)
                    .key_phrase_backfill_needed()
            })
            .unwrap_or(false)
    };
    if backfill_needed {
        let owned_state = state.clone();
        let backfill = tokio::task::spawn_blocking(move || {
            let (docs, script) = owned_state
                .service
                .read()
                .map(|service| {
                    service
                        .memory_store(MemoryStore::Workspace)
                        .key_phrase_backfill_snapshot()
                })
                .map_err(|_| anyhow::anyhow!("memory service lock poisoned"))?;
            if docs.is_empty() {
                return Ok(0);
            }
            let raw = MemoryService::extract_key_phrases_for_docs(&docs, script.as_deref());
            write_workspace_mutation(&owned_state, move |service| {
                Ok(service.apply_key_phrase_backfill(&docs, raw))
            })
        })
        .await
        .map_err(|join| anyhow::anyhow!("backfill task failed: {join}"))
        .and_then(|inner| inner);
        if let Err(error) = backfill {
            tracing::warn!("key-phrase backfill failed (fail-open): {error:#}");
        }
    }
    if let Some(worker) = &state.staged_writer {
        let started = Instant::now();
        let result = worker
            .read_view()
            .and_then(|view| match request.filter_ast.as_ref() {
                Some(filter_ast) => view.search_cached_with_filter_ast(request.request, filter_ast),
                None => view.search_cached(request.request),
            });
        state.telemetry.record_query(
            started.elapsed().as_millis() as u64,
            result.is_err(),
            result.as_ref().map(|r| r.data.is_empty()).unwrap_or(false),
        );
        return match result {
            Ok(response) => Json(response).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    let started = Instant::now();
    let result = match state.service.read() {
        Ok(service) => {
            let memory = service.memory_store(MemoryStore::Workspace);
            match request.filter_ast.as_ref() {
                Some(filter_ast) => {
                    memory.search_cached_with_filter_ast(request.request, filter_ast)
                }
                None => memory.search_cached(request.request),
            }
        }
        Err(_) => Err(anyhow::anyhow!("memory service lock poisoned")),
    };
    match result {
        Ok(result) => {
            state.telemetry.record_query(
                started.elapsed().as_millis() as u64,
                false,
                result.data.is_empty(),
            );
            Json(serde_json::to_value(result).unwrap()).into_response()
        }
        Err(_) => {
            state
                .telemetry
                .record_query(started.elapsed().as_millis() as u64, true, false);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":"search failed"})),
            )
                .into_response()
        }
    }
}

pub(super) async fn get_memory(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(memory_id): Path<String>,
    Query(query): Query<GetRequest>,
) -> impl IntoResponse {
    let request = GetRequest { memory_id, ..query };
    let value = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let result = match state.service.read() {
        Ok(service) => service.memory_store(MemoryStore::Captured).get(request),
        Err(_) => Err(anyhow::anyhow!("memory reader lock poisoned")),
    };
    match result {
        Ok(Some(memory)) => Json(memory).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":"memory lookup failed"})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct ListMemoriesQuery {
    user_id: String,
    session_id: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    #[serde(default)]
    include_inactive: bool,
    filter_ast: Option<String>,
}

pub(super) async fn list_memories(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Query(query): Query<ListMemoriesQuery>,
) -> impl IntoResponse {
    let filter_ast = match query.filter_ast.as_deref() {
        Some(encoded) => match serde_json::from_str::<Mem0FilterExpr>(encoded) {
            Ok(ast) => Some(ast),
            Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        },
        None => None,
    };
    if filter_ast
        .as_ref()
        .is_some_and(|ast| ast.validate_mem0_fields().is_err())
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let request = ListRequest {
        user_id: query.user_id,
        session_id: query.session_id,
        limit: query.limit.unwrap_or(100),
        cursor: query.cursor,
        include_inactive: query.include_inactive,
    };
    let value = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let result = match state.service.read() {
        Ok(service) => service
            .memory_store(MemoryStore::Captured)
            .list_with_filter_ast(request, filter_ast.as_ref()),
        Err(_) => Err(anyhow::anyhow!("memory reader lock poisoned")),
    };
    match result {
        Ok(result) => Json(result).into_response(),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"invalid list request"})),
        )
            .into_response(),
    }
}

pub(super) async fn update_memory(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(memory_id): Path<String>,
    Json(mut request): Json<UpdateRequestWithMetadata>,
) -> impl IntoResponse {
    request.request.memory_id = memory_id;
    let value = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            write_mutation(&state, move |service| service.update_with_metadata(request))
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")))
    };
    match result {
        Ok(Some(memory)) => Json(memory).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::UNPROCESSABLE_ENTITY),
            Json(json!({"detail":"memory update failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn delete_memory(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(memory_id): Path<String>,
    Query(query): Query<DeleteRequest>,
) -> impl IntoResponse {
    let request = DeleteRequest {
        doc_id: memory_id,
        ..query
    };
    let value = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            write_mutation(&state, move |service| {
                service.delete(&request.user_id, &request.doc_id)
            })
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")))
    };
    match result {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"detail":"memory deletion failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn refresh_memories(State(state): State<AppState>) -> impl IntoResponse {
    if let Some(worker) = &state.staged_writer {
        return match worker.flush().await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(status) => status.into_response(),
        };
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            write_mutation(&state, move |service| service.refresh())
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")))
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":"memory refresh failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn delete(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let request = match serde_json::from_value::<DeleteRequest>(value) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid request"})),
            )
                .into_response()
        }
    };
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            write_mutation(&state, move |s| s.delete(&request.user_id, &request.doc_id))
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")))
    };
    match result {
        Ok(affected) => Json(json!({"success":true,"affected":affected as usize})).into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"detail":"mutation failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn supersede(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let request = match serde_json::from_value::<SupersedeRequest>(value) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid request"})),
            )
                .into_response()
        }
    };
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            write_mutation(&state, move |s| {
                s.supersede(&request.user_id, &request.replacement_id, &request.old_id)
            })
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")))
    };
    match result {
        Ok(affected) => Json(json!({"success":true,"affected":affected as usize})).into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"detail":"mutation failed"})),
        )
            .into_response(),
    }
}

pub(super) async fn expire(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(value): Json<Value>,
) -> impl IntoResponse {
    if let Err(error) = tenant_check(&state, &value, auth.as_ref().map(|a| &a.0)) {
        return error.into_response();
    }
    let Ok(writer) = state.writer_gate.clone().try_lock_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"writer busy"})),
        )
            .into_response();
    };
    let user_id = value
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let state_for_mutation = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _writer = writer;
        write_mutation(&state_for_mutation, move |service| service.expire(&user_id))
    })
    .await
    .unwrap_or_else(|error| Err(anyhow::anyhow!("mutation task failed: {error}")));
    match result {
        Ok(affected) => Json(json!({"success":true,"affected":affected})).into_response(),
        Err(error) => (
            server_writer::mutation_status(&error, StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"detail":"mutation failed"})),
        )
            .into_response(),
    }
}
