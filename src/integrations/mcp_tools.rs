use crate::index::SearchResult;
use crate::integrations::mcp_transport::{JsonRpcError, JsonRpcResponse, ToolDefinition};
use crate::memory_api::MemoryService;
use crate::source::SourceDocument;
use membrick_extension_api::{
    CapabilityId, DocumentQuery, ExtensionContext, ExtensionDocument, ExtensionError,
    ExtensionHttpRequest, ExtensionHttpResponse, ExtensionRouteMatch, InvocationIdentity,
    ScopedDocumentStore,
};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;

pub(crate) fn provider_filter_values() -> Vec<&'static str> {
    crate::provider_sources::PROVIDERS
        .iter()
        .map(|provider| provider.name)
        // ChatGPT connects through the generic resident MCP bridge rather
        // than a Membrick-owned capture hook/runtime integration.
        .chain(std::iter::once("chatgpt"))
        .collect()
}

/// Selects where an integration sends memory operations. Production entry
/// points construct with `Resident`; in-process behavior is selected only by
/// explicit unit-test fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryBackend {
    Resident,
    InProcess,
}

impl MemoryBackend {
    pub(crate) const fn is_resident(self) -> bool {
        matches!(self, Self::Resident)
    }
}

/// Whether the resident backend has either half of the searchable project
/// view on disk. Lifecycle hooks use this to avoid creating empty stores just
/// to run their default session-start query.
pub(crate) fn has_search_store(root: &Path) -> bool {
    let membrick_root = root.join(".membrick");
    membrick_root
        .join(crate::integrations::mcp_index::SHARED_MEMORY_DIR)
        .is_dir()
        || membrick_root
            .join(crate::memory_api::workspace::WORKSPACE_MEMORY_NAME)
            .is_dir()
}

fn resident_text_response(id: Option<Value>, text: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({"content":[{"type":"text","text":text}]})),
        error: None,
    }
}

fn resident_error_response(id: Option<Value>, code: i64, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    }
}

/// Common updater action exposed by every provider-local MCP server. The host
/// client owns the approval prompt; the updater still validates the cached
/// target version and install source before changing the installation.
pub(crate) fn update_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "update_membrick".to_string(),
        description: "Update Membrick to the version shown in its update notice. Ask the user for approval before calling this tool; it runs the package manager or replaces the managed Membrick binary.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "version": {"type": "string", "description": "The exact available version shown in the Membrick update notice."}
            },
            "required": ["version"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn call_update_tool(
    id: Option<Value>,
    arguments: &Value,
) -> anyhow::Result<JsonRpcResponse> {
    if let Some(name) = arguments
        .as_object()
        .and_then(|object| object.keys().find(|key| key.as_str() != "version"))
    {
        return Ok(resident_error_response(
            id,
            -32602,
            &format!("unknown update_membrick argument: {name}"),
        ));
    }
    let version = arguments
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty());
    let Some(version) = version else {
        return Ok(resident_error_response(
            id,
            -32602,
            "update_membrick requires the version shown in the update notice",
        ));
    };
    let message = match crate::updater::install_cached_update(version) {
        Ok(message) => message,
        Err(error) => format!("Membrick update was not installed: {error:#}"),
    };
    Ok(resident_text_response(id, &message))
}

/// Forward workspace memory tools to the resident Membrick server.
/// Provider MCP processes must not open a second workspace/provider index.
pub(crate) fn call_resident_tool(
    id: Option<Value>,
    root: &Path,
    provider: &str,
    tool: &str,
    arguments: &Value,
) -> anyhow::Result<JsonRpcResponse> {
    let root = root.canonicalize()?;
    let workspace = root.to_string_lossy().into_owned();
    let client = match crate::memory_client::Memory::new(&root) {
        Ok(client) => client,
        Err(error) => return Ok(resident_error_response(id, -32602, &error.to_string())),
    };
    let context = crate::memory_client::AgentOperationContext {
        workspace: Some(workspace),
        provider: provider.to_owned(),
        session_id: None,
    };
    let may_mutate = extension_tool_is_mutating(tool)
        || matches!(
            tool,
            "add_memory" | "delete_memory" | "update_memory" | "refresh"
        );
    let (mut body, transport) = match client.call_workspace_tool_with_transport(
        &context,
        tool,
        arguments.clone(),
        may_mutate,
    ) {
        Ok(result) => result,
        Err(error) => {
            let code = match &error {
                crate::memory_client::MemoryError::Unavailable { .. } => -32002,
                crate::memory_client::MemoryError::Uncertain { .. } => -32003,
                crate::memory_client::MemoryError::Rejected { .. }
                | crate::memory_client::MemoryError::Configuration { .. } => -32602,
            };
            return Ok(resident_error_response(id, code, &error.to_string()));
        }
    };
    if tool == "info" {
        let server = body
            .as_object_mut()
            .expect("successful info response is an object")
            .entry("server")
            .or_insert_with(|| json!({}));
        server["status"] = json!("connected");
        server["transport"] = json!(transport);
    }
    Ok(resident_text_response(
        id,
        &serde_json::to_string_pretty(&body)?,
    ))
}

/// Record every provider MCP `tools/call` after its provider-facing response is
/// formed. This remains separate from optional transcript/session recording.
pub(crate) fn record_mcp_tool_response(
    root: &Path,
    provider: &str,
    tool: &str,
    request_id: Option<Value>,
    arguments: &Value,
    duration_ms: u64,
    response: &mut anyhow::Result<JsonRpcResponse>,
) {
    let mut session_attribution = None;
    let prepared_trace = if tool == "search" {
        crate::telemetry::mcp_trace_id_for_call(provider, tool, &request_id, arguments)
            .ok()
            .and_then(|(timestamp_ms, trace_id)| {
                let response = response.as_mut().ok()?;
                if response.error.is_some() {
                    return None;
                }
                let text = response
                    .result
                    .as_mut()?
                    .get_mut("content")?
                    .as_array_mut()?
                    .first_mut()?
                    .get_mut("text")?
                    .as_str()?
                    .to_string();
                let mut body = serde_json::from_str::<Value>(&text).ok()?;
                let object = body.as_object_mut()?;
                session_attribution = object
                    .remove("__membrick_telemetry")
                    .and_then(|metadata| {
                        let session_id = metadata
                            .get("session_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let attribution = metadata
                            .get("session_attribution")
                            .and_then(Value::as_str)?
                            .to_string();
                        matches!(attribution.as_str(), "explicit" | "inferred" | "absent")
                            .then_some((session_id, attribution))
                    });
                object.insert("search_id".to_string(), json!(trace_id));
                object.insert(
                    "feedback_guidance".to_string(),
                    json!("If these results do not answer the question or miss important context, call search_feedback with this search_id, an overall label, and one relevance judgment for every returned doc_id. Use relevant, partially_relevant, or not_relevant. Use enrich_doc separately if a source document needs improvement. If the results are adequate, no feedback call is needed."),
                );
                let encoded = serde_json::to_string_pretty(&body).ok()?;
                *response
                    .result
                    .as_mut()?
                    .get_mut("content")?
                    .as_array_mut()?
                    .first_mut()?
                    .get_mut("text")? = Value::String(encoded);
                Some((timestamp_ms, trace_id))
            })
    } else {
        None
    };
    if tool == "search"
        && prepared_trace.is_none()
        && response
            .as_ref()
            .is_ok_and(|response| response.error.is_none())
    {
        let id = response
            .as_ref()
            .ok()
            .and_then(|response| response.id.clone());
        *response = Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32603,
                message: "search results could not be assigned a review ID; retry the search"
                    .to_string(),
            }),
        });
    }
    let (error, payload) = match response {
        Ok(response) => (
            response.error.is_some(),
            json!({"result":response.result.clone(), "error":response.error.clone()}),
        ),
        Err(error) => (true, json!({"transport_error":format!("{error:#}")})),
    };
    let record = match prepared_trace {
        Some((timestamp_ms, trace_id)) => {
            if let Some((session_id, attribution)) = session_attribution {
                crate::telemetry::record_mcp_call_with_trace_id_and_attribution(
                    root,
                    provider,
                    tool,
                    request_id,
                    arguments,
                    duration_ms,
                    error,
                    payload,
                    timestamp_ms,
                    trace_id,
                    session_id,
                    &attribution,
                )
            } else {
                crate::telemetry::record_mcp_call_with_trace_id(
                    root,
                    provider,
                    tool,
                    request_id,
                    arguments,
                    duration_ms,
                    error,
                    payload,
                    timestamp_ms,
                    trace_id,
                )
            }
        }
        None => crate::telemetry::record_mcp_call(
            root,
            provider,
            tool,
            request_id,
            arguments,
            duration_ms,
            error,
            payload,
        ),
    };
    if let Err(error) = record {
        tracing::warn!("Membrick could not append MCP call history: {error:#}");
        if tool == "search"
            && response
                .as_ref()
                .is_ok_and(|response| response.error.is_none())
        {
            let id = response
                .as_ref()
                .ok()
                .and_then(|response| response.id.clone());
            *response = Ok(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32603,
                    message: format!(
                        "search results could not be saved for review; retry the search ({error:#})"
                    ),
                }),
            });
        }
    }
}

/// Save capture work in the resident before returning from the provider hook.
#[cfg(any(feature = "claude-code", feature = "codex"))]
pub(crate) fn submit_transcript_capture(
    root: &Path,
    provider: &str,
    document_type: &str,
    input: Value,
) -> anyhow::Result<()> {
    let root = root.canonicalize()?;
    let client = crate::Memory::new(&root).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    client
        .submit_capture(
            &crate::agent::AgentOperationContext {
                workspace: Some(root.to_string_lossy().into_owned()),
                provider: provider.to_owned(),
                session_id: input
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            crate::agent::CaptureRequest {
                document_type: document_type.to_owned(),
                input,
            },
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

/// Publish a hook capture through the resident workspace owner.
pub(crate) fn upsert_hook_document(
    root: &Path,
    provider: &str,
    session_id: &str,
    document: crate::SourceDocument,
) -> anyhow::Result<()> {
    upsert_hook_documents(root, provider, session_id, vec![document])
}

/// Publish a batch of hook captures through the resident workspace owner's
/// single upsert path. Returns once the batch is durable.
pub(crate) fn upsert_hook_documents(
    root: &Path,
    provider: &str,
    session_id: &str,
    documents: Vec<crate::SourceDocument>,
) -> anyhow::Result<()> {
    upsert_hook_documents_with_backend(
        root,
        provider,
        session_id,
        documents,
        MemoryBackend::Resident,
    )
}

pub(crate) fn upsert_hook_documents_with_backend(
    root: &Path,
    provider: &str,
    session_id: &str,
    documents: Vec<crate::SourceDocument>,
    backend: MemoryBackend,
) -> anyhow::Result<()> {
    if !backend.is_resident() {
        let _ = (provider, session_id);
        return MemoryService::with_shared_memory(root, |service| {
            for document in documents {
                service.upsert(document);
            }
            service.refresh_index()
        });
    }
    let root = root.canonicalize()?;
    let client = crate::Memory::new(&root).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    client
        .publish_capture_documents(
            &crate::agent::AgentOperationContext {
                workspace: Some(root.to_string_lossy().into_owned()),
                provider: provider.to_owned(),
                session_id: Some(session_id.to_owned()),
            },
            session_id,
            documents,
            None,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

pub(crate) fn replace_hook_documents(
    root: &Path,
    provider: &str,
    session_id: &str,
    key: &str,
    value: &str,
    documents: Vec<crate::SourceDocument>,
) -> anyhow::Result<()> {
    replace_hook_documents_with_backend(
        root,
        provider,
        session_id,
        key,
        value,
        documents,
        MemoryBackend::Resident,
    )
}

pub(crate) fn replace_hook_documents_with_backend(
    root: &Path,
    provider: &str,
    session_id: &str,
    key: &str,
    value: &str,
    documents: Vec<crate::SourceDocument>,
    backend: MemoryBackend,
) -> anyhow::Result<()> {
    if !backend.is_resident() {
        return MemoryService::with_shared_memory(root, |service| {
            service.replace_source_group(key, value, documents)?;
            service.refresh_index()
        });
    }
    let root = root.canonicalize()?;
    let client = crate::Memory::new(&root).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    client
        .publish_capture_documents(
            &crate::agent::AgentOperationContext {
                workspace: Some(root.to_string_lossy().into_owned()),
                provider: provider.to_owned(),
                session_id: Some(session_id.to_owned()),
            },
            session_id,
            documents,
            Some((key.to_owned(), value.to_owned())),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

/// Search the resident published view for a hook retrieval request.
pub(crate) fn search_hook_documents(
    root: &Path,
    provider: &str,
    session_id: Option<&str>,
    query: &str,
    top_k: usize,
) -> anyhow::Result<Value> {
    search_hook_documents_with_backend(
        root,
        provider,
        session_id,
        query,
        top_k,
        MemoryBackend::Resident,
    )
}

pub(crate) fn search_hook_documents_with_backend(
    root: &Path,
    provider: &str,
    session_id: Option<&str>,
    query: &str,
    top_k: usize,
    backend: MemoryBackend,
) -> anyhow::Result<Value> {
    if !backend.is_resident() {
        let ignore_paths = [
            "node_modules",
            "target",
            "dist",
            "build",
            "vendor",
            "coverage",
            ".git",
        ]
        .map(str::to_string);
        return MemoryService::open_workspace(
            root,
            crate::integrations::mcp_index::SHARED_MEMORY_DIR,
            &ignore_paths,
            || {
                let input = crate::adapters::AdapterInput {
                    root,
                    max_bytes: 5_000_000,
                    max_files: 50_000,
                    max_depth: 20,
                    max_total_bytes: 100_000_000,
                };
                let graph = crate::adapters::build_project_graph(&input)?;
                let graph = crate::adapters::apply_ignore_paths(graph, &ignore_paths);
                Ok(crate::adapters::graph_to_source_documents(&graph))
            },
        )
        .and_then(|mut service| {
            let results = service.search_for_hook(query, provider, session_id, top_k)?;
            Ok(filter_automatic_context_results(search_results_for_query(
                &service, query, results,
            )))
        });
    }
    let root = root.canonicalize()?;
    let client = crate::Memory::new(&root).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let response = client
        .workspace_search(
            &crate::agent::AgentOperationContext {
                workspace: Some(root.to_string_lossy().into_owned()),
                provider: provider.to_owned(),
                session_id: session_id
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned),
            },
            query,
            top_k,
        )
        .map_err(|error| anyhow::anyhow!("resident search failed: {error}"))?;
    Ok(filter_automatic_context_results(serde_json::to_value(
        response,
    )?))
}

/// JSON Schema fragment for the optional `provider` search argument.
pub(crate) fn provider_argument_schema() -> Value {
    let providers = provider_filter_values();
    json!({
        "type": "string",
        "description": format!("Restrict results to memories captured by one provider ({}). Omit to search the shared pool.", providers.join(", ")),
        "enum": providers,
    })
}

/// Build the document filters for an MCP search from the optional `provider`
/// argument. An empty map searches the whole shared pool; an unknown provider
/// is a caller error, not an empty result.
pub(crate) fn search_provider_filters(
    arguments: &Value,
) -> Result<BTreeMap<String, String>, String> {
    let providers = provider_filter_values();
    let provider = arguments
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty());
    let Some(provider) = provider else {
        return Ok(BTreeMap::new());
    };
    if !providers.contains(&provider) {
        return Err(format!(
            "unknown provider \"{provider}\"; expected one of: {}",
            providers.join(", ")
        ));
    }
    Ok(BTreeMap::from([(
        "provider".to_string(),
        provider.to_string(),
    )]))
}

/// Extract the optional `session_id` search argument. A supplied value must be
/// a non-empty, non-blank identifier; omit the argument for stateless search.
/// A present-but-blank value, or a present non-string value, is rejected
/// rather than silently treated as absent, so callers cannot accidentally
/// lose session state. This is a conversation-state key only, never a corpus
/// filter.
pub(crate) fn search_session_id(arguments: &Value) -> Result<Option<String>, String> {
    let Some(value) = arguments.get("session_id") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(session_id) = value.as_str() else {
        return Err("session_id must be a string".to_string());
    };
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err("session_id must not be blank".to_string());
    }
    if session_id.len() > 256 {
        return Err("session_id must be at most 256 bytes".to_string());
    }
    if session_id.chars().any(|character| character.is_control()) {
        return Err("session_id must not contain control characters".to_string());
    }
    Ok(Some(session_id.to_string()))
}

/// Resolve the effective session id for an MCP search.
///
/// An explicit `session_id` argument always wins. When the argument is
/// absent (or null), the search inherits the session most recently seen
/// active in this workspace — hooks keep that pointer current through the
/// service's conversation-state store, so an agent that never passes
/// `session_id` still gets follow-up resolution against its live
/// conversation. A present-but-blank argument is still rejected rather than
/// falling back, so callers cannot accidentally lose session state. The
/// Explicit ids route only that request and do not change lifecycle state.
/// Only provider hooks mark sessions active, so an MCP call cannot make a
/// completed or unrelated session appear live.
#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "roo-runtime"
))]
pub(crate) fn resolve_search_session_id(
    arguments: &Value,
    service: &MemoryService,
    provider: &str,
) -> Result<Option<String>, String> {
    match search_session_id(arguments)? {
        Some(explicit) => Ok(Some(explicit)),
        None => {
            let active = service.current_session_id(provider);
            if let Some(session_id) = active.as_deref() {
                service.note_active_session(provider, session_id);
            }
            Ok(active)
        }
    }
}

/// Format retrieval hits for an agent. Keep this separate from the internal
/// ranking representation: diagnostics and score components are useful while
/// tuning the index, but distract an agent from the memory itself.
pub fn search_results(service: &MemoryService, results: Vec<SearchResult>) -> Value {
    format_search_results(service, None, results)
}

pub(crate) fn search_results_for_query(
    service: &MemoryService,
    query: &str,
    results: Vec<SearchResult>,
) -> Value {
    format_search_results(service, Some(query), results)
}

fn format_search_results(
    service: &MemoryService,
    query: Option<&str>,
    results: Vec<SearchResult>,
) -> Value {
    let comparison_operands = query
        .map(|query| service.comparison_operands_for_query(query))
        .filter(|operands| operands.len() == 2)
        .unwrap_or_default();
    let comparison_coverage = (!comparison_operands.is_empty()).then(|| {
        let sides = comparison_operands
            .iter()
            .map(|operand| {
                let count = results
                    .iter()
                    .filter(|result| result.comparison_operands.contains(operand))
                    .count();
                json!({
                    "operand": operand,
                    "retrieved_documents": count,
                    "covered": count > 0,
                })
            })
            .collect::<Vec<_>>();
        let covered = sides
            .iter()
            .filter(|side| side.get("covered").and_then(Value::as_bool) == Some(true))
            .count();
        json!({"sides": sides, "covered_sides": covered, "total_sides": comparison_operands.len()})
    });
    let results = results
        .into_iter()
        .enumerate()
        .filter_map(|(rank, result)| {
            let document = service.source_document_by_id(&result.doc_id)?;
            let content: String = match query {
                Some(query) => crate::integrations::recall::relevant_excerpt(
                    &document.content,
                    query,
                    &result.matched_terms,
                    4_000,
                ),
                None => document.content.chars().take(4_000).collect(),
            };
            let content = preserve_matched_excerpt(
                &document.content,
                &content,
                query,
                &result.matched_terms,
                4_000,
            );
            // Anchor relative date expressions ("yesterday", "last month") to
            // an absolute date the agent can see. Fail-open: memories without
            // a timestamp keep their original text.
            let timestamp_kind = document.timestamp_kind();
            let timestamp = document
                .timestamp
                .as_deref()
                .and_then(crate::temporal::normalize_timestamp);
            let content = match (timestamp_kind, timestamp.as_deref()) {
                (crate::source::TimestampKind::Session, Some(date)) => {
                    format!("[session date: {date}]\n{content}")
                }
                (crate::source::TimestampKind::UserSupplied, Some(date)) => {
                    format!("[memory date: {date}]\n{content}")
                }
                _ => content,
            };
            let created_at = matches!(
                timestamp_kind,
                crate::source::TimestampKind::Session | crate::source::TimestampKind::UserSupplied
            )
            .then(|| timestamp.clone())
            .flatten();
            let modified_at = (timestamp_kind == crate::source::TimestampKind::SourceModified)
                .then(|| timestamp.clone())
                .flatten();
            let event_at = (timestamp_kind == crate::source::TimestampKind::WorkspaceEvent)
                .then(|| timestamp.clone())
                .flatten();
            Some(json!({
                "id": result.doc_id,
                "doc_id": result.doc_id,
                "rank": rank + 1,
                "source_hash": crate::source::source_document_hash(&document).ok(),
                "source": result.source,
                "content": content,
                "score": result.score,
                "created_at": created_at,
                "modified_at": modified_at,
                "event_at": event_at,
                "timestamp_kind": timestamp_kind.as_str(),
                "session_id": document.group_id.clone(),
                "matched_terms": result.matched_terms,
                "matched_entities": result.matched_entities,
                "semantic_status": result.semantic_status,
                "superseded_by": result.superseded_by,
                "relation_confidence": result.relation_confidence,
                "relation_evidence": result.relation_evidence,
                "comparison_operands": result.comparison_operands,
                "claim_states": service.semantic_claim_states(&result.doc_id),
                "document_type": document.filters.get("document_type"),
                "branch": document.filters.get("branch"),
                "revision": document.filters.get("revision"),
            }))
        })
        .collect::<Vec<_>>();
    match comparison_coverage {
        Some(comparison) => json!({"results": results, "comparison": comparison}),
        None => json!({"results": results}),
    }
}

fn preserve_matched_excerpt(
    content: &str,
    excerpt: &str,
    query: Option<&str>,
    matched_terms: &[String],
    max_bytes: usize,
) -> String {
    let lower_content = content.to_ascii_lowercase();
    let lower_excerpt = excerpt.to_ascii_lowercase();
    let query_match = query
        .map(str::trim)
        .filter(|query| query.len() >= 3)
        .map(str::to_ascii_lowercase)
        .filter(|query| lower_content.contains(query) && !lower_excerpt.contains(query));
    let term_match = matched_terms.iter().find_map(|term| {
        let term = term.trim();
        let folded = term.to_ascii_lowercase();
        (folded.len() >= 3 && lower_content.contains(&folded) && !lower_excerpt.contains(&folded))
            .then_some(folded)
    });
    let Some(needle) = query_match.or(term_match) else {
        return excerpt.to_string();
    };
    let Some(match_start) = lower_content.find(&needle) else {
        return excerpt.to_string();
    };
    let mut start = match_start.saturating_sub(max_bytes / 4);
    while !content.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + max_bytes).min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content[start..end].to_string()
}

/// Conflicted captures remain available through explicit MCP search, where
/// the caller can inspect their evidence. Automatic hook context must not
/// promote their competing claims into project guidance.
pub(crate) fn automatic_context_allowed(result: &Value) -> bool {
    let conflicted = result.get("semantic_status").and_then(Value::as_str) == Some("conflicted");
    let unprovenanced_legacy_file_change = result
        .get("source")
        .and_then(Value::as_str)
        .is_some_and(|source| source.starts_with("lint-ai://workspace/file-change/"))
        && result.get("branch").is_none_or(Value::is_null)
        && result.get("revision").is_none_or(Value::is_null);

    !conflicted && !unprovenanced_legacy_file_change
}

/// Filter shared automatic-hook retrieval before any provider formats context.
/// Explicit MCP search remains unfiltered so callers can inspect conflicts and
/// legacy file-change history.
fn filter_automatic_context_results(mut payload: Value) -> Value {
    if let Some(results) = payload.get_mut("results").and_then(Value::as_array_mut) {
        results.retain(automatic_context_allowed);
    }
    payload
}

/// Return a bounded, provider-neutral view of the indexed memories.
pub(crate) fn list_memories(service: &MemoryService, limit: usize) -> Value {
    let memories = service
        .source_documents()
        .into_iter()
        .filter(|document| is_recorded_memory(document))
        .take(limit.clamp(1, 100))
        .map(|document| {
            json!({
                "source": document.source,
                "document_type": document.concept,
                "group_id": document.group_id,
                "content": document.content.chars().take(2_000).collect::<String>(),
            })
        })
        .collect::<Vec<_>>();
    json!({"count": memories.len(), "memories": memories})
}

fn is_recorded_memory(document: &SourceDocument) -> bool {
    crate::provider_sources::is_provider_source(&document.source)
        || crate::provider_sources::INTERNAL_MEMORY_SOURCE_PREFIXES
            .iter()
            .any(|prefix| document.source.starts_with(prefix))
        || document
            .filters
            .get("source_type")
            .is_some_and(|source_type| source_type == "recorded-session")
        || document.group_id.as_deref().is_some_and(|group_id| {
            crate::provider_sources::PROVIDERS
                .iter()
                .any(|provider| group_id.starts_with(provider.session_tag))
        })
}

pub(crate) fn list_memories_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "list_memories".to_string(),
        description: "List indexed memories with bounded content previews.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
            },
            "additionalProperties": false
        }),
    }
}

// ---------------------------------------------------------------------------
// Agent bulletin board tools.
// ---------------------------------------------------------------------------

/// Tool definitions gathered from all registered extensions.
pub(crate) fn extension_tool_definitions() -> Vec<ToolDefinition> {
    crate::extension_host::registry().tool_definitions()
}

pub(crate) fn is_extension_tool_name(name: &str) -> bool {
    crate::extension_host::registry().has_tool_name(name)
}

pub(crate) fn extension_tool_is_mutating(name: &str) -> bool {
    crate::extension_host::registry()
        .tool_is_mutating_name(name)
        .unwrap_or(false)
}

type SharedExtensionService<'a> = Rc<RefCell<&'a mut MemoryService>>;

struct McpExtensionDocuments<'a> {
    service: SharedExtensionService<'a>,
    owner: String,
    workspace: String,
    extension_id: String,
}

impl McpExtensionDocuments<'_> {
    fn storage_id(&self, id: &str) -> String {
        format!(
            "extension:{}:{}:{id}",
            self.extension_id.len(),
            self.extension_id
        )
    }

    fn matches_scope(&self, document: &SourceDocument) -> bool {
        let filters = &document.filters;
        filters.get("doc_kind").map(String::as_str) == Some("extension_document")
            && filters.get("extension_id") == Some(&self.extension_id)
            && filters.get("extension_owner") == Some(&self.owner)
            && filters.get("extension_workspace") == Some(&self.workspace)
    }

    fn decode(
        document: &SourceDocument,
        extension_id: &str,
    ) -> Result<ExtensionDocument, ExtensionError> {
        let is_generic_extension_document = document
            .filters
            .get("doc_kind")
            .is_some_and(|kind| kind == "extension_document");
        let kind = document
            .filters
            .get("extension_kind")
            .or_else(|| document.filters.get("doc_kind"))
            .cloned()
            .unwrap_or_else(|| document.source.clone());
        let value = if is_generic_extension_document {
            serde_json::from_str(document.content.as_ref()).map_err(|error| {
                ExtensionError::new(format!("invalid stored extension document: {error}"))
            })?
        } else {
            // Preserve the complete legacy record as opaque generic JSON so
            // its owning extension can migrate its own persisted schema.
            serde_json::to_value(document).map_err(|error| {
                ExtensionError::new(format!("failed to encode legacy document: {error}"))
            })?
        };
        Ok(ExtensionDocument {
            id: document
                .doc_id
                .strip_prefix(&format!(
                    "extension:{}:{}:",
                    extension_id.len(),
                    extension_id
                ))
                .unwrap_or(&document.doc_id)
                .to_string(),
            kind,
            value,
        })
    }
}

impl ScopedDocumentStore for McpExtensionDocuments<'_> {
    fn get(&self, id: &str) -> Result<Option<ExtensionDocument>, ExtensionError> {
        let storage_id = self.storage_id(id);
        let service = self.service.borrow();
        let found = service
            .source_documents()
            .into_iter()
            .find(|document| document.doc_id == storage_id && self.matches_scope(document));
        found
            .map(|document| Self::decode(document, &self.extension_id))
            .transpose()
    }

    fn list(&self, kind: Option<&str>) -> Result<Vec<ExtensionDocument>, ExtensionError> {
        let service = self.service.borrow();
        service
            .source_documents()
            .into_iter()
            .filter(|document| {
                self.matches_scope(document)
                    && kind.is_none_or(|kind| {
                        document.filters.get("extension_kind").map(String::as_str) == Some(kind)
                    })
            })
            .map(|document| Self::decode(document, &self.extension_id))
            .collect()
    }

    fn search(&self, query: &DocumentQuery) -> Result<Vec<ExtensionDocument>, ExtensionError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let text = query.text.as_deref().map(str::to_lowercase);
        let mut documents = self
            .service
            .borrow()
            .source_documents()
            .into_iter()
            .filter(|document| self.matches_scope(document))
            .map(|document| Self::decode(&document, &self.extension_id))
            .filter(|document| {
                document.as_ref().is_ok_and(|document| {
                    query
                        .kind
                        .as_deref()
                        .is_none_or(|kind| document.kind == kind)
                        && query
                            .id_prefix
                            .as_deref()
                            .is_none_or(|prefix| document.id.starts_with(prefix))
                        && text.as_deref().is_none_or(|needle| {
                            document.id.to_lowercase().contains(needle)
                                || document.kind.to_lowercase().contains(needle)
                                || document.value.to_string().to_lowercase().contains(needle)
                        })
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        documents.sort_by(|left, right| left.id.cmp(&right.id));
        documents.truncate(query.limit);
        Ok(documents)
    }

    fn put_batch(&mut self, documents: Vec<ExtensionDocument>) -> Result<(), ExtensionError> {
        let mut ids = std::collections::HashSet::new();
        if documents.iter().any(|document| {
            document.id.trim().is_empty()
                || document.kind.trim().is_empty()
                || !ids.insert(document.id.clone())
        }) {
            return Err(ExtensionError::new(
                "extension document IDs and kinds must be nonempty and IDs unique",
            ));
        }
        let mut service = self.service.borrow_mut();
        let existing = service.source_documents();
        if documents.iter().any(|document| {
            let storage_id = self.storage_id(&document.id);
            existing
                .iter()
                .any(|stored| stored.doc_id == storage_id && !self.matches_scope(stored))
        }) {
            return Err(ExtensionError::new(
                "extension document ID conflicts with a document outside its scope",
            ));
        }
        let source_documents = documents
            .into_iter()
            .map(|document| {
                let mut filters = HashMap::new();
                filters.insert("doc_kind".to_string(), "extension_document".to_string());
                filters.insert("extension_owner".to_string(), self.owner.clone());
                filters.insert("extension_workspace".to_string(), self.workspace.clone());
                filters.insert("extension_id".to_string(), self.extension_id.clone());
                filters.insert("extension_kind".to_string(), document.kind.clone());
                let storage_id = self.storage_id(&document.id);
                let content = serde_json::to_string(&document.value).map_err(|error| {
                    ExtensionError::new(format!("failed to encode extension document: {error}"))
                })?;
                Ok(SourceDocument {
                    doc_id: storage_id,
                    source: format!("extension:{}", document.kind),
                    content: content.into(),
                    concept: document.kind,
                    group_id: None,
                    headings: vec![],
                    links: vec![],
                    timestamp: None,
                    doc_length: 0,
                    author_agent: None,
                    filters: filters.into_iter().collect(),
                    key_phrases: vec![],
                    key_phrase_extraction_hash: String::new(),
                })
            })
            .collect::<Result<Vec<_>, ExtensionError>>()?;
        service
            .stage_upsert_batch(source_documents)
            .map_err(|error| {
                ExtensionError::new(format!("failed to persist extension documents: {error:#}"))
            })?;
        Ok(())
    }
}

/// Outcome of moving records stored before an extension owned them into
/// that extension's scoped storage.
#[derive(Debug, Default)]
pub(crate) struct LegacyAdoption {
    pub(crate) adopted: usize,
    pub(crate) left_in_place: usize,
    pub(crate) backup: Option<std::path::PathBuf>,
}

/// Move records this workspace stored before an extension owned them into
/// the extension's scoped storage, then remove the originals.
///
/// Each extension decides which of its legacy kinds are in scope and how to
/// convert them. The originals are written to a JSON Lines file in
/// `backup_dir` first and removed only after every converted document reads
/// back unchanged. A record its extension declines or cannot convert stays in
/// place. Running again is harmless: adopted originals are gone, and writing
/// a converted document again is an idempotent upsert.
pub(crate) fn adopt_legacy_extension_records(
    service: &mut MemoryService,
    owner: &str,
    workspace: &str,
    backup_dir: &Path,
) -> anyhow::Result<LegacyAdoption> {
    let adopters = crate::extension_host::registry()
        .legacy_record_adopters()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut report = LegacyAdoption::default();
    if adopters.is_empty() {
        return Ok(report);
    }
    let legacy = service
        .source_documents()
        .into_iter()
        .filter(|document| {
            document
                .filters
                .get("doc_kind")
                .is_some_and(|kind| adopters.contains_key(kind.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut adopted = BTreeMap::<String, Vec<(SourceDocument, ExtensionDocument)>>::new();
    for document in legacy {
        let registration = adopters[document.filters["doc_kind"].as_str()];
        let record = serde_json::to_value(&document)?;
        match registration
            .handler
            .adopt_legacy_record(owner, workspace, &record)
        {
            Ok(Some(converted)) => adopted
                .entry(registration.descriptor.id.to_string())
                .or_default()
                .push((document, converted)),
            Ok(None) => report.left_in_place += 1,
            Err(error) => {
                tracing::warn!(
                    doc_id = %document.doc_id,
                    "could not adopt legacy extension record: {error}"
                );
                report.left_in_place += 1;
            }
        }
    }
    if adopted.is_empty() {
        return Ok(report);
    }

    std::fs::create_dir_all(backup_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let backup = backup_dir.join(format!("legacy-extension-records-{stamp}.jsonl"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create_new(&backup)?;
        for (original, _) in adopted.values().flatten() {
            serde_json::to_writer(&mut file, original)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    report.backup = Some(backup);

    let shared = Rc::new(RefCell::new(service));
    for (extension_id, documents) in &adopted {
        let mut store = McpExtensionDocuments {
            service: Rc::clone(&shared),
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            extension_id: extension_id.clone(),
        };
        store
            .put_batch(
                documents
                    .iter()
                    .map(|(_, converted)| converted.clone())
                    .collect(),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        for (_, converted) in documents {
            let stored = store
                .get(&converted.id)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            anyhow::ensure!(
                stored.as_ref() == Some(converted),
                "adopted extension document {} did not read back unchanged",
                converted.id
            );
        }
    }
    let mut service = shared.borrow_mut();
    for (original, _) in adopted.values().flatten() {
        service.remove(&original.doc_id);
        report.adopted += 1;
    }
    service.flush()?;
    Ok(report)
}

struct McpExtensionContext<'a> {
    service: SharedExtensionService<'a>,
    owner: String,
    workspace: String,
    provider: String,
    session_id: Option<String>,
    documents: McpExtensionDocuments<'a>,
}

impl ExtensionContext for McpExtensionContext<'_> {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn documents(&mut self) -> &mut dyn ScopedDocumentStore {
        &mut self.documents
    }

    fn scoped_documents(
        &mut self,
        scope: &membrick_extension_api::ExtensionScope,
    ) -> &mut dyn ScopedDocumentStore {
        self.documents.extension_id = scope.extension_id().as_str().to_owned();
        &mut self.documents
    }

    fn invoke(
        &mut self,
        capability: CapabilityId,
        operation: &str,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let registry = crate::extension_host::registry();
        registry.invoke(self, capability, operation, input)
    }

    fn invocation_identity(&self) -> InvocationIdentity<'_> {
        InvocationIdentity {
            provider: &self.provider,
            principal: None,
            caller_extension: None,
        }
    }
}

pub(crate) fn dispatch_extension_tool(
    tool: &str,
    arguments: &Value,
    service: &mut MemoryService,
    owner: &str,
    workspace: &str,
    provider: &str,
) -> Result<Value, String> {
    dispatch_extension_tool_with_session(tool, arguments, service, owner, workspace, provider, None)
}

pub(crate) fn dispatch_extension_tool_with_session(
    tool: &str,
    arguments: &Value,
    service: &mut MemoryService,
    owner: &str,
    workspace: &str,
    provider: &str,
    session_id: Option<&str>,
) -> Result<Value, String> {
    let service = Rc::new(RefCell::new(service));
    let mut context = McpExtensionContext {
        service: Rc::clone(&service),
        owner: owner.to_string(),
        workspace: workspace.to_string(),
        provider: provider.to_string(),
        session_id: session_id.map(str::to_string),
        documents: McpExtensionDocuments {
            service,
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            extension_id: String::new(),
        },
    };
    crate::extension_host::registry()
        .call_named(&mut context, tool, arguments.clone())
        .map_err(|error| error.to_string())
}

pub(crate) fn dispatch_extension_http_route(
    route: &ExtensionRouteMatch,
    request: ExtensionHttpRequest,
    service: &mut MemoryService,
    owner: &str,
    workspace: &str,
    provider: &str,
) -> Result<ExtensionHttpResponse, ExtensionError> {
    let service = Rc::new(RefCell::new(service));
    let mut context = McpExtensionContext {
        service: Rc::clone(&service),
        owner: owner.to_string(),
        workspace: workspace.to_string(),
        provider: provider.to_string(),
        session_id: None,
        documents: McpExtensionDocuments {
            service,
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            extension_id: String::new(),
        },
    };
    crate::extension_host::registry().call_http_route(&mut context, route, request)
}

/// Stable memory owner for agent-added memories over MCP.
use crate::MCP_MEMORY_USER_ID;

/// MCP tool definitions for direct memory writes and reads.
pub(crate) fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "add_memory".to_string(),
            description: "Record or correct a memory: a fact, decision, or observation worth remembering. Returns the request_id. Pass a unique request_id so retries are safe. To correct an older memory, pass its memory_id in supersedes_id.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "The memory content, e.g. \"The API rate limit is 100 requests per minute.\""},
                    "request_id": {"type": "string", "description": "Unique ID for this write; reuse it when retrying."},
                    "session_id": {"type": "string", "description": "Conversation session ID. Defaults to the session most recently seen active in this workspace."},
                    "role": {"type": "string", "description": "Message role: \"user\" or \"assistant\". Defaults to \"assistant\".", "enum": ["user", "assistant"]},
                    "supersedes_id": {"type": "string", "description": "Memory ID this new memory corrects or replaces."}
                },
                "required": ["content", "request_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_memory".to_string(),
            description: "Get one stored memory by its memory ID (from search results or add_memory). Returns the full memory record, or an error when the ID is unknown.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "Memory ID to fetch."},
                    "include_inactive": {"type": "boolean", "description": "Also return memories that were superseded or expired. Defaults to false.", "default": false}
                },
                "required": ["memory_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "delete_memory".to_string(),
            description: "Forget one memory you own. Use a memory_id returned by search, get_memory, or add_memory.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "Memory ID to permanently delete."}
                },
                "required": ["memory_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "review_search".to_string(),
            description: "Review recorded MCP searches through the resident's in-memory lookup index. The first request may return status=indexing while it rebuilds from telemetry; retry shortly. Omit search_id to page through unreviewed searches with cursor and page_size. Supply search_id to retrieve that exact query, result, and available source documents. Reading a page does not mark a search reviewed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "search_id": {"type":"string", "description":"Return one exact recorded search. Omit to list unreviewed searches."},
                    "cursor": {"type":"string", "description":"Continuation cursor returned by the previous page."},
                    "page_size": {"type":"integer", "minimum":1, "maximum":100, "default":10}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "search_feedback".to_string(),
            description: "Submit an overall usefulness label and a relevance judgment for every document returned by one search_id. The original rank is taken from the recorded search result. This records evaluation feedback only; use enrich_doc to change a specific source document.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "search_id": {"type":"string"},
                    "label": {"type":"string", "enum":["helpful","partially_helpful","not_helpful"]},
                    "feedback": {"type":"string", "description":"What answered the query, what was missing or misleading, and why this label applies."},
                    "document_judgments": {"type":"array", "description":"Provide exactly one relevance judgment for every document in the recorded search results. The service records each document's original rank and source hash; do not submit a replacement rank.", "items":{"type":"object", "properties":{
                        "doc_id":{"type":"string"},
                        "relevance":{"type":"string", "enum":["relevant","partially_relevant","not_relevant"]},
                        "feedback":{"type":"string", "description":"Optional brief reason for this document's relevance judgment."}
                    }, "required":["doc_id","relevance"], "additionalProperties":false}},
                    "query_rule_additions": {"type":"object", "description":"Optional additions to supported query-only rule lists. Every term must appear in the reviewed query or returned source documents. Supported lists: query_time_units, query_count_phrases, query_option_connectors, query_option_prefixes, query_option_phrases, query_comparison_markers, query_temporal_filter_markers, query_both_verbs, query_aggregate_phrases, query_current_state_markers, query_explanation_terms.", "properties": {
                        "query_time_units":{"type":"array", "items":{"type":"string"}},
                        "query_count_phrases":{"type":"array", "items":{"type":"string"}},
                        "query_option_connectors":{"type":"array", "items":{"type":"string"}},
                        "query_option_prefixes":{"type":"array", "items":{"type":"string"}},
                        "query_option_phrases":{"type":"array", "items":{"type":"string"}},
                        "query_comparison_markers":{"type":"array", "items":{"type":"string"}},
                        "query_temporal_filter_markers":{"type":"array", "items":{"type":"string"}},
                        "query_both_verbs":{"type":"array", "items":{"type":"string"}},
                        "query_aggregate_phrases":{"type":"array", "items":{"type":"string"}},
                        "query_current_state_markers":{"type":"array", "items":{"type":"string"}},
                        "query_explanation_terms":{"type":"array", "items":{"type":"string"}}
                    }, "additionalProperties":false}
                },
                "required":["search_id","label","feedback","document_judgments"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "enrich_doc".to_string(),
            description: "Add evidence-backed searchable metadata to one existing source document. Use the exact doc_id and expected_source_hash returned by search or review_search. This changes the selected document only; it does not label a search.".to_string(),
            input_schema: json!({
                "type":"object",
                "properties": {
                    "doc_id":{"type":"string","description":"Exact source document ID to enrich."},
                    "expected_source_hash":{"type":"string","description":"Exact source_hash returned for this document by search or review_search."},
                    "key_phrases":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"kind":{"type":"string","enum":["event","place","org","work","thing"]}},"required":["text","kind"],"additionalProperties":false}},
                    "session_conclusion":{"type":"string","description":"Replace the managed conclusion on an existing session-context document."},
                    "headings":{"type":"array","items":{"type":"string"}},
                    "links":{"type":"array","items":{"type":"string"}},
                    "supersedes_document_id":{"type":"string","description":"Existing same-scope document this document explicitly replaces; cite it in evidence."},
                    "evidence":{"type":"array","minItems":1,"items":{"type":"object","properties":{"doc_id":{"type":"string"},"expected_source_hash":{"type":"string"}},"required":["doc_id","expected_source_hash"],"additionalProperties":false}}
                },
                "required":["doc_id","expected_source_hash","evidence"],
                "additionalProperties":false
            }),
        },
    ]
}

/// Dispatch an `add_memory`, `get_memory`, or `delete_memory` tool call. Returns the JSON
/// result payload, or an error string for invalid arguments.
///
/// These wrap the same `MemoryService::add` / `MemoryService::get` the
/// HTTP server exposes, with the stable `"mcp"` memory owner. Writes run
/// the full enrichment pipeline; reads enforce ownership and visibility.
pub(crate) fn dispatch_memory_tool(
    tool: &str,
    arguments: &Value,
    service: &mut MemoryService,
    provider: &str,
) -> Result<Value, String> {
    use crate::memory_api::GetRequest;

    fn get_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
        args.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{name} is required"))
    }

    let allowed: &[&str] = match tool {
        "add_memory" => &[
            "content",
            "request_id",
            "session_id",
            "role",
            "supersedes_id",
        ],
        "get_memory" => &["memory_id", "include_inactive"],
        "delete_memory" => &["memory_id"],
        _ => return Err(format!("unknown memory tool: {tool}")),
    };
    if let Some(obj) = arguments.as_object() {
        if let Some(unknown) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("unknown {tool} argument: {unknown}"));
        }
    }

    match tool {
        "add_memory" => {
            let request = build_add_memory_request(arguments, service, provider)?;
            let response = service.add(request).map_err(|e| e.to_string())?;
            serde_json::to_value(&response).map_err(|e| e.to_string())
        }
        "get_memory" => {
            let memory_id = get_str(arguments, "memory_id")?;
            let include_inactive = arguments
                .get("include_inactive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let request = GetRequest {
                user_id: MCP_MEMORY_USER_ID.to_string(),
                memory_id: memory_id.to_string(),
                include_inactive,
            };
            match service.get(request).map_err(|e| e.to_string())? {
                Some(record) => serde_json::to_value(&record).map_err(|e| e.to_string()),
                None => Err(format!("memory not found: {memory_id}")),
            }
        }
        "delete_memory" => {
            let memory_id = get_str(arguments, "memory_id")?;
            let deleted = service
                .delete(MCP_MEMORY_USER_ID, memory_id)
                .map_err(|e| e.to_string())?;
            Ok(json!({"memory_id": memory_id, "deleted": deleted}))
        }
        _ => Err(format!("unknown memory tool: {tool}")),
    }
}

pub(crate) fn build_add_memory_request(
    arguments: &Value,
    service: &MemoryService,
    provider: &str,
) -> Result<crate::memory_api::AddRequest, String> {
    use crate::memory_api::{AddRequest, Message};
    if arguments.as_object().is_some_and(|args| {
        args.keys().any(|key| {
            ![
                "content",
                "request_id",
                "session_id",
                "role",
                "supersedes_id",
            ]
            .contains(&key.as_str())
        })
    }) {
        return Err("unknown add_memory argument".to_string());
    }
    let content = arguments
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "content is required".to_string())?;
    let request_id = arguments
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "request_id is required".to_string())?;
    let role = arguments
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("assistant");
    if role != "user" && role != "assistant" {
        return Err("role must be \"user\" or \"assistant\"".to_string());
    }
    let session_id = resolve_search_session_id(arguments, service, provider)?
        .ok_or_else(|| "no active session: pass session_id to add_memory".to_string())?;
    Ok(AddRequest {
        request_id: request_id.to_string(),
        messages: vec![Message {
            role: role.to_string(),
            timestamp: None,
            content: content.to_string(),
            expires_at_ms: None,
            supersedes_id: arguments
                .get("supersedes_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        }],
        user_id: MCP_MEMORY_USER_ID.to_string(),
        session_id,
    })
}

// ---------------------------------------------------------------------------
// Shared tools/call dispatch for registered extension tools and Membrick's
// memory tools, used by every MCP adapter that opts in.
// ---------------------------------------------------------------------------

/// Route one `tools/call` invocation for a registered extension or a
/// Membrick memory tool, then wrap the payload in the standard MCP
/// text-content envelope (or a `-32602` JSON-RPC error). `service` must be an
/// initialized memory service; this syncs the shared memory store first.
///
/// Extension and memory write tools run against the persistent shared store
/// under the cross-process write lock — the composed
/// view is in-memory only, so view writes would vanish on process exit and
/// stay invisible to hooks and other providers. Reads stay on the view.
#[cfg(test)]
pub(crate) fn call_mcp_tool(
    tool_name: &str,
    id: Option<Value>,
    arguments: &Value,
    service: &mut MemoryService,
    root: &Path,
    provider: &str,
) -> anyhow::Result<JsonRpcResponse> {
    call_mcp_tool_with_backend(
        tool_name,
        id,
        arguments,
        service,
        root,
        provider,
        MemoryBackend::InProcess,
    )
}

pub(crate) fn call_mcp_tool_with_backend(
    tool_name: &str,
    id: Option<Value>,
    arguments: &Value,
    service: &mut MemoryService,
    root: &Path,
    provider: &str,
    backend: MemoryBackend,
) -> anyhow::Result<JsonRpcResponse> {
    #[cfg(test)]
    if !backend.is_resident() && tool_name != "enrich_doc" {
        service.sync_shared_memory(&crate::integrations::mcp_index::shared_memory_root(root))?;
    }
    #[cfg(not(test))]
    let _ = backend;
    // Workspace-scoped extension documents use the stable MCP owner.
    let workspace = root.to_string_lossy().to_string();
    let is_write = extension_tool_is_mutating(tool_name)
        || matches!(
            tool_name,
            "add_memory" | "delete_memory" | "search_feedback" | "enrich_doc"
        );
    let result: Result<Value, String> = if is_write {
        // Resolve the session from the composed service, which tracks hook
        // activity, then run the write against the persistent shared-memory
        // service (a fresh service never sees hook-tracked sessions).
        let mut write_arguments = arguments.clone();
        let invocation_session_id = service.current_session_id(provider);
        if !is_extension_tool_name(tool_name) {
            let resolved_session_id = resolve_search_session_id(arguments, service, provider)
                .map_err(anyhow::Error::msg)?;
            if write_arguments.get("session_id").is_none() {
                if let Some(session_id) = resolved_session_id.as_deref() {
                    write_arguments["session_id"] = json!(session_id);
                }
            }
        }
        if tool_name == "search_feedback" {
            // Feedback must use the resident's search-history locator and
            // review-state caches. Opening a temporary shared-memory service
            // here gives feedback an empty locator map and makes a just
            // recorded search look like it is still indexing (or absent).
            service
                .search_feedback_mcp(root, &write_arguments)
                .map_err(|error| format!("{error:#}"))
        } else {
            match crate::memory_api::MemoryService::with_shared_memory(root, |store| {
                let result = if is_extension_tool_name(tool_name) {
                    dispatch_extension_tool_with_session(
                        tool_name,
                        arguments,
                        store,
                        "mcp",
                        &workspace,
                        provider,
                        invocation_session_id.as_deref(),
                    )
                } else {
                    dispatch_core_write_tool(tool_name, &write_arguments, store, provider, root)
                };
                result.map_err(anyhow::Error::msg)
            }) {
                Ok(value) => Ok(value),
                Err(error) => Err(format!("{error:#}")),
            }
        }
    } else {
        match tool_name {
            name if is_extension_tool_name(name) => {
                let session_id = service.current_session_id(provider);
                dispatch_extension_tool_with_session(
                    tool_name,
                    arguments,
                    service,
                    "mcp",
                    &workspace,
                    provider,
                    session_id.as_deref(),
                )
            }
            "add_memory" | "get_memory" | "delete_memory" => {
                dispatch_memory_tool(tool_name, arguments, service, provider)
            }
            "review_search" => service
                .review_search_mcp(root, arguments)
                .map_err(|error| format!("{error:#}")),
            _ => Err(format!("unknown tool: {tool_name}")),
        }
    };
    #[cfg(test)]
    if is_write && tool_name != "search_feedback" && result.is_ok() {
        service.sync_shared_memory(&crate::integrations::mcp_index::shared_memory_root(root))?;
    }
    match result {
        Ok(payload) => Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&payload)?,
                }]
            })),
            error: None,
        }),
        Err(message) => Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32602,
                message,
            }),
        }),
    }
}

/// Dispatch a host-owned write tool against an explicitly passed service.
/// Registered extension writes use `dispatch_extension_tool` directly.
fn dispatch_core_write_tool(
    tool_name: &str,
    arguments: &Value,
    service: &mut MemoryService,
    provider: &str,
    root: &Path,
) -> Result<Value, String> {
    match tool_name {
        "add_memory" => dispatch_memory_tool(tool_name, arguments, service, provider),
        "delete_memory" => dispatch_memory_tool(tool_name, arguments, service, provider),
        "search_feedback" => service
            .search_feedback_mcp(root, arguments)
            .map_err(|error| format!("{error:#}")),
        "enrich_doc" => service
            .enrich_doc_mcp(arguments)
            .map_err(|error| format!("{error:#}")),
        _ => Err(format!("unknown write tool: {tool_name}")),
    }
}

// Codex validates arguments locally because its protocol already exposes the
// unknown argument name; Claude and Gemini use this shared parser instead.
#[cfg_attr(
    not(any(feature = "claude-code", feature = "gemini-cli")),
    allow(dead_code)
)]
pub(crate) fn parse_list_memories_limit(arguments: &Value) -> Result<usize, &'static str> {
    if arguments
        .as_object()
        .map(|object| object.keys().any(|key| key != "limit"))
        .unwrap_or(false)
    {
        return Err("unknown list_memories argument");
    }
    Ok(arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{IndexStore, PipelineOptions};
    use crate::source::SourceDocument;
    use std::collections::BTreeMap;

    #[test]
    fn automatic_context_excludes_conflicted_claims_only() {
        assert!(automatic_context_allowed(
            &json!({"semantic_status":"current"})
        ));
        assert!(automatic_context_allowed(&json!({"semantic_status":null})));
        assert!(automatic_context_allowed(
            &json!({"document_type":"session-context","semantic_status":null})
        ));
        assert!(automatic_context_allowed(
            &json!({"document_type":"session-observation","semantic_status":null})
        ));
        assert!(!automatic_context_allowed(
            &json!({"semantic_status":"conflicted"})
        ));
    }

    #[test]
    fn automatic_hook_search_removes_conflicted_hits_and_keeps_other_results() {
        let payload = json!({"results":[
            {"id":"current","semantic_status":"current"},
            {"id":"conflicted","semantic_status":"conflicted"},
            {"id":"legacy","semantic_status":null}
        ]});
        let filtered = filter_automatic_context_results(payload);
        let results = filtered["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["id"], "current");
        assert_eq!(results[1]["id"], "legacy");
    }

    fn memory_service() -> MemoryService {
        MemoryService::in_memory(PipelineOptions::default())
    }

    #[test]
    fn extension_document_namespaces_isolate_sibling_extensions() {
        let mut service = memory_service();
        let shared = Rc::new(RefCell::new(&mut service));
        let mut alpha = McpExtensionDocuments {
            service: Rc::clone(&shared),
            owner: "owner".into(),
            workspace: "/workspace".into(),
            extension_id: "alpha".into(),
        };
        let mut beta = McpExtensionDocuments {
            service: Rc::clone(&shared),
            owner: "owner".into(),
            workspace: "/workspace".into(),
            extension_id: "beta".into(),
        };
        alpha
            .put_batch(vec![ExtensionDocument {
                id: "shared-id".into(),
                kind: "record".into(),
                value: json!({"value":"alpha secret"}),
            }])
            .unwrap();
        assert!(beta.get("shared-id").unwrap().is_none());
        beta.put_batch(vec![ExtensionDocument {
            id: "shared-id".into(),
            kind: "record".into(),
            value: json!({"value":"beta secret"}),
        }])
        .unwrap();
        assert_eq!(
            alpha.get("shared-id").unwrap().unwrap().value["value"],
            "alpha secret"
        );
        assert_eq!(
            beta.get("shared-id").unwrap().unwrap().value["value"],
            "beta secret"
        );
        assert_eq!(alpha.list(None).unwrap().len(), 1);
        assert_eq!(beta.list(None).unwrap().len(), 1);

        let service = shared.borrow();
        let stored = service.source_documents();
        assert!(stored.iter().any(|doc| {
            doc.doc_id == "extension:5:alpha:shared-id"
                && doc.filters.get("extension_id").map(String::as_str) == Some("alpha")
                && doc.filters.get("extension_owner").map(String::as_str) == Some("owner")
                && doc.filters.get("extension_workspace").map(String::as_str) == Some("/workspace")
        }));
        assert!(stored.iter().any(|doc| {
            doc.doc_id == "extension:4:beta:shared-id"
                && doc.filters.get("extension_id").map(String::as_str) == Some("beta")
        }));
    }

    #[test]
    #[test]
    fn memory_tool_definitions_expose_add_and_get() {
        let defs = memory_tool_definitions();
        let names: Vec<_> = defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "add_memory",
                "get_memory",
                "delete_memory",
                "review_search",
                "search_feedback",
                "enrich_doc"
            ]
        );
        // Schemas require only the essentials.
        assert_eq!(
            defs[0].input_schema["required"],
            json!(["content", "request_id"])
        );
        assert_eq!(defs[1].input_schema["required"], json!(["memory_id"]));
        assert_eq!(defs[2].input_schema["required"], json!(["memory_id"]));
        assert_eq!(defs[4].input_schema["properties"].get("updates"), None);
        assert!(defs[4].input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "document_judgments"));
        assert_eq!(
            defs[5].input_schema["required"],
            json!(["doc_id", "expected_source_hash", "evidence"])
        );
    }

    #[test]
    fn shared_router_runs_review_search_then_search_feedback_for_one_search() {
        let root = std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("target")
            .join(format!(
                "membrick-shared-search-review-{}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let result = json!({"results":[{"id":"review-doc","content":"The writer publishes accepted document changes."}]});
        let recorded_response = json!({
            "result":{"content":[{"text":serde_json::to_string(&result).unwrap()}]}
        });
        crate::telemetry::record_mcp_call(
            &root,
            "codex",
            "search",
            Some(json!(91)),
            &json!({"query":"how are document changes published?"}),
            5,
            false,
            recorded_response,
        )
        .unwrap();

        let mut service = memory_service();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let page = loop {
            let page = call_mcp_tool(
                "review_search",
                Some(json!(92)),
                &json!({"page_size":10}),
                &mut service,
                &root,
                "codex",
            )
            .unwrap();
            let body = page.result.as_ref().unwrap()["content"][0]["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            if body.as_ref().and_then(|body| body.get("status")).is_none() {
                break page;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "search history index did not become ready: {body:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let page_text = page.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let page_body: Value = serde_json::from_str(&page_text).unwrap();
        let search_id = page_body["searches"][0]["search_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(page_body["searches"].as_array().unwrap().len(), 1);
        assert_eq!(
            page_body["searches"][0]["search"]["arguments"]["query"],
            "how are document changes published?"
        );

        let exact = call_mcp_tool(
            "review_search",
            Some(json!(93)),
            &json!({"search_id":search_id}),
            &mut service,
            &root,
            "codex",
        )
        .unwrap();
        let exact_text = exact.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let exact_body: Value = serde_json::from_str(&exact_text).unwrap();
        assert_eq!(exact_body["search_id"], search_id);
        assert_eq!(exact_body["reviewed"], false);
        assert_eq!(
            exact_body["search"]["arguments"]["query"],
            "how are document changes published?"
        );
        assert_eq!(exact_body["result"]["results"][0]["id"], "review-doc");

        let feedback = call_mcp_tool(
            "search_feedback",
            Some(json!(94)),
            &json!({
                "search_id":search_id,
                "label":"helpful",
                "feedback":"The result answered the question with the publication behavior.",
                "document_judgments":[{"doc_id":"review-doc","relevance":"relevant"}]
            }),
            &mut service,
            &root,
            "codex",
        )
        .unwrap();
        let feedback_payload = feedback.result.unwrap_or_else(|| {
            panic!(
                "search_feedback router returned error: {:?}",
                feedback.error
            )
        });
        let feedback_text = feedback_payload["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let feedback_body: Value = serde_json::from_str(&feedback_text).unwrap();
        assert_eq!(feedback_body["status"], "reviewed");
        assert_eq!(feedback_body["search_id"], search_id);

        let reviewed = call_mcp_tool(
            "review_search",
            Some(json!(95)),
            &json!({"search_id":search_id}),
            &mut service,
            &root,
            "codex",
        )
        .unwrap();
        let reviewed_text = reviewed.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let reviewed_body: Value = serde_json::from_str(&reviewed_text).unwrap();
        assert_eq!(reviewed_body["reviewed"], true);

        let target = SourceDocument {
            doc_id: "router-enrich-target".to_string(),
            source: "test://router-enrich-target".to_string(),
            content: "The target contains a project decision.".into(),
            concept: "decision".to_string(),
            group_id: None,
            filters: Default::default(),
            headings: Vec::new(),
            links: Vec::new(),
            timestamp: None,
            doc_length: 39,
            author_agent: None,
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        };
        let evidence = SourceDocument {
            doc_id: "router-enrich-evidence".to_string(),
            source: "test://router-enrich-evidence".to_string(),
            content: "The supporting evidence confirms the project decision.".into(),
            concept: "evidence".to_string(),
            group_id: None,
            filters: Default::default(),
            headings: Vec::new(),
            links: Vec::new(),
            timestamp: None,
            doc_length: 55,
            author_agent: None,
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        };
        crate::memory_api::MemoryService::with_shared_memory(&root, |shared| {
            shared.upsert(target.clone());
            shared.upsert(evidence.clone());
            shared.flush()?;
            Ok(())
        })
        .unwrap();
        let (target_hash, evidence_hash) =
            crate::memory_api::MemoryService::with_shared_memory(&root, |shared| {
                Ok((
                    crate::source::source_document_hash(
                        shared.source_document_by_id(&target.doc_id).unwrap(),
                    )?,
                    crate::source::source_document_hash(
                        shared.source_document_by_id(&evidence.doc_id).unwrap(),
                    )?,
                ))
            })
            .unwrap();
        let enriched = call_mcp_tool(
            "enrich_doc",
            Some(json!(96)),
            &json!({
                "doc_id": target.doc_id,
                "expected_source_hash": target_hash,
                "key_phrases": [{"text":"project decision", "kind":"thing"}],
                "evidence": [{"doc_id": evidence.doc_id, "expected_source_hash": evidence_hash}]
            }),
            &mut service,
            &root,
            "codex",
        )
        .unwrap();
        let enriched_payload = enriched
            .result
            .unwrap_or_else(|| panic!("enrich_doc router returned error: {:?}", enriched.error));
        let enriched_text = enriched_payload["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let enriched_body: Value = serde_json::from_str(&enriched_text).unwrap();
        assert_eq!(enriched_body["status"], "enriched");
        assert_eq!(
            enriched_body["changed_document_ids"],
            json!([target.doc_id])
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_response_id_matches_the_recorded_search_id() {
        let root = std::env::temp_dir().join(format!(
            "membrick-search-response-id-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut response = Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id: Some(json!(7)),
            result: Some(json!({
                "content":[{"type":"text","text":"{\"results\":[{\"id\":\"doc-1\"}],\"__membrick_telemetry\":{\"session_id\":\"active-session\",\"session_attribution\":\"inferred\"}}"}]
            })),
            error: None,
        });
        record_mcp_tool_response(
            &root,
            "claude",
            "search",
            Some(json!(7)),
            &json!({"query":"example"}),
            3,
            &mut response,
        );

        let response = response.unwrap();
        let text = response.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let body: Value = serde_json::from_str(&text).unwrap();
        let response_search_id = body["search_id"].as_str().unwrap();
        assert!(body.get("__membrick_telemetry").is_none());
        assert!(body["feedback_guidance"]
            .as_str()
            .unwrap()
            .contains("search_feedback"));

        let history =
            std::fs::read_to_string(root.join(".membrick/mcp-call-history.jsonl")).unwrap();
        let record: crate::telemetry::McpCallRecord =
            serde_json::from_str(history.lines().next().unwrap()).unwrap();
        assert_eq!(record.tool, "search");
        assert_eq!(record.session_id.as_deref(), Some("active-session"));
        assert_eq!(record.session_attribution, "inferred");
        assert_eq!(record.trace_id.as_deref(), Some(response_search_id));
        assert_eq!(
            record
                .response
                .pointer("/result/content/0/text")
                .and_then(Value::as_str),
            Some(text.as_str())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_search_is_not_returned_when_history_cannot_be_appended() {
        let root = std::env::temp_dir().join(format!(
            "membrick-search-history-failure-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".membrick/mcp-call-history.lock")).unwrap();
        let mut response = Ok(JsonRpcResponse {
            jsonrpc: "2.0",
            id: Some(json!(9)),
            result: Some(json!({
                "content":[{"type":"text","text":"{\"results\":[{\"id\":\"doc-1\"}]}"}]
            })),
            error: None,
        });

        record_mcp_tool_response(
            &root,
            "codex",
            "search",
            Some(json!(9)),
            &json!({"query":"example"}),
            2,
            &mut response,
        );

        let response = response.unwrap();
        assert!(response.error.is_some(), "unrecorded success was returned");
        assert!(response.result.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dispatch_memory_tool_rejects_unknown_tool_and_args() {
        let mut service = memory_service();
        let err = dispatch_memory_tool("nope", &json!({}), &mut service, "claude").unwrap_err();
        assert!(err.contains("unknown memory tool"), "{err}");
        let err = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "x", "request_id": "r1", "bogus": 1}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("unknown add_memory argument"), "{err}");
    }

    #[test]
    fn add_memory_then_get_memory_round_trip() {
        let mut service = memory_service();
        let payload = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "The API rate limit is 100 requests per minute.",
                    "request_id": "mem-1", "session_id": "s1"}),
            &mut service,
            "claude",
        )
        .unwrap();
        assert_eq!(payload["success"], json!(true));
        assert_eq!(payload["request_id"], json!("mem-1"));

        // The memory ID is deterministic: stable_doc_id("mcp:mem-1:0").
        let memory_id = crate::memory_api::memory_document_id("mcp", "mem-1", 0);
        let record = dispatch_memory_tool(
            "get_memory",
            &json!({"memory_id": memory_id}),
            &mut service,
            "claude",
        )
        .unwrap();
        assert!(record["content"].as_str().unwrap().contains("rate limit"));

        // Unknown ID is an error, not an empty result.
        let err = dispatch_memory_tool(
            "get_memory",
            &json!({"memory_id": "does-not-exist"}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("memory not found"), "{err}");

        let deleted = dispatch_memory_tool(
            "delete_memory",
            &json!({"memory_id": memory_id}),
            &mut service,
            "claude",
        )
        .unwrap();
        assert_eq!(deleted["deleted"], json!(true));
        let deleted_again = dispatch_memory_tool(
            "delete_memory",
            &json!({"memory_id": memory_id}),
            &mut service,
            "claude",
        )
        .unwrap();
        assert_eq!(deleted_again["deleted"], json!(false));
    }

    #[test]
    fn add_memory_can_explicitly_correct_an_older_memory() {
        let mut service = memory_service();
        let first = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "The API rate limit is 100 requests per minute.",
                    "request_id": "old-limit", "session_id": "s1"}),
            &mut service,
            "claude",
        )
        .unwrap();
        let old_id = crate::memory_api::memory_document_id("mcp", "old-limit", 0);
        let second = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "The API rate limit is 200 requests per minute.",
                    "request_id": "new-limit", "session_id": "s1",
                    "supersedes_id": old_id}),
            &mut service,
            "claude",
        )
        .unwrap();
        assert_eq!(first["success"], json!(true));
        assert_eq!(second["success"], json!(true));
        let filters = BTreeMap::from([("memory_user_id".to_string(), "mcp".to_string())]);
        let results = service
            .search_with_filters("API rate limit", "test", None, 10, &filters, None)
            .unwrap();
        assert!(results
            .iter()
            .all(|result| result.doc_id
                != crate::memory_api::memory_document_id("mcp", "old-limit", 0)));
    }

    #[test]
    fn add_memory_request_id_retry_is_idempotent() {
        let mut service = memory_service();
        let args = json!({"content": "same content", "request_id": "dup-1",
                          "session_id": "s1"});
        let first = dispatch_memory_tool("add_memory", &args, &mut service, "claude").unwrap();
        let second = dispatch_memory_tool("add_memory", &args, &mut service, "claude").unwrap();
        assert_eq!(first, second);
        // Same request_id with different content is still a conflict.
        let err = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "different content", "request_id": "dup-1",
                    "session_id": "s1"}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("already used with different content"), "{err}");
    }

    #[test]
    fn add_memory_requires_session() {
        let mut service = memory_service();
        // No session_id arg and no hook-tracked session: clear error.
        let err = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "x", "request_id": "r1"}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("session_id"), "{err}");
    }

    #[test]
    fn add_memory_validates_role_and_content() {
        let mut service = memory_service();
        let err = dispatch_memory_tool(
            "add_memory",
            &json!({"content": "x", "request_id": "r1", "session_id": "s1", "role": "system"}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("role"), "{err}");
        let err = dispatch_memory_tool(
            "add_memory",
            &json!({"request_id": "r1", "session_id": "s1"}),
            &mut service,
            "claude",
        )
        .unwrap_err();
        assert!(err.contains("content is required"), "{err}");
    }

    #[test]
    fn search_provider_filters_accepts_known_providers() {
        let filters = search_provider_filters(&json!({"query": "x", "provider": "codex"})).unwrap();
        assert_eq!(
            filters,
            BTreeMap::from([("provider".to_string(), "codex".to_string())])
        );
    }

    #[test]
    fn search_provider_filters_defaults_to_shared_pool() {
        assert!(search_provider_filters(&json!({"query": "x"}))
            .unwrap()
            .is_empty());
        assert!(
            search_provider_filters(&json!({"query": "x", "provider": ""}))
                .unwrap()
                .is_empty()
        );
        assert!(
            search_provider_filters(&json!({"query": "x", "provider": "  "}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_provider_filters_rejects_unknown_provider() {
        let error =
            search_provider_filters(&json!({"query": "x", "provider": "clippy"})).unwrap_err();
        assert!(error.contains("unknown provider"), "{error}");
        assert!(error.contains("codex"), "{error}");
    }

    const LEGACY_NOTE_KIND: &str = "test_legacy_note";
    const LEGACY_NOTE_EXTENSION: &str = "membrick-test-legacy-notes";

    /// Test-only extension that adopts `test_legacy_note` records whose
    /// `note_workspace` filter matches the workspace being opened.
    struct LegacyNoteExtension;

    impl membrick_extension_api::ExtensionHandler for LegacyNoteExtension {
        fn legacy_record_kinds(&self) -> &'static [&'static str] {
            &[LEGACY_NOTE_KIND]
        }

        fn adopt_legacy_record(
            &self,
            _owner: &str,
            workspace: &str,
            record: &Value,
        ) -> Result<Option<ExtensionDocument>, membrick_extension_api::ExtensionError> {
            if record["filters"]["note_workspace"].as_str() != Some(workspace) {
                return Ok(None);
            }
            Ok(Some(ExtensionDocument {
                id: record["doc_id"].as_str().unwrap_or_default().to_string(),
                kind: "note".to_string(),
                value: json!({"text": record["content"]}),
            }))
        }

        fn call(
            &self,
            _context: &mut dyn membrick_extension_api::ExtensionContext,
            _tool: membrick_extension_api::ToolId,
            _input: Value,
        ) -> Result<Value, membrick_extension_api::ExtensionError> {
            Err(membrick_extension_api::ExtensionError::new(
                "the legacy note test extension has no tools",
            ))
        }
    }

    static LEGACY_NOTE_DESCRIPTOR: membrick_extension_api::ExtensionDescriptor =
        membrick_extension_api::ExtensionDescriptor {
            id: membrick_extension_api::ExtensionId::new(LEGACY_NOTE_EXTENSION),
            provides: &[membrick_extension_api::CapabilityId::new(
                "test-legacy-notes",
            )],
            requires: &[],
            tools: &[],
        };
    static LEGACY_NOTE_HANDLER: LegacyNoteExtension = LegacyNoteExtension;

    #[membrick_extension_api::distributed_slice(membrick_extension_api::EXTENSIONS)]
    static LEGACY_NOTE_REGISTRATION: membrick_extension_api::ExtensionRegistration =
        membrick_extension_api::ExtensionRegistration {
            descriptor: &LEGACY_NOTE_DESCRIPTOR,
            handler: &LEGACY_NOTE_HANDLER,
        };

    #[test]
    fn legacy_records_move_into_their_extension_once() {
        fn note(doc_id: &str, text: &str, workspace: &str) -> SourceDocument {
            SourceDocument {
                doc_id: doc_id.to_string(),
                source: "legacy-notes".to_string(),
                doc_length: text.len(),
                content: text.into(),
                concept: "legacy record".to_string(),
                group_id: None,
                filters: [
                    ("doc_kind".to_string(), LEGACY_NOTE_KIND.to_string()),
                    ("note_workspace".to_string(), workspace.to_string()),
                ]
                .into_iter()
                .collect(),
                headings: vec![],
                links: vec![],
                timestamp: Some("2026-10-20T00:00:00Z".to_string()),
                author_agent: Some("codex".to_string()),
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            }
        }

        let workspace = "/legacy/workspace";
        let first = note("legacy-note:1", "First step done.", workspace);
        let second = note("legacy-note:2", "Second step done.", workspace);
        let foreign = note("legacy-note:3", "Elsewhere.", "/other/workspace");
        let mut service = memory_service();
        service
            .stage_upsert_batch(vec![first.clone(), second.clone(), foreign.clone()])
            .unwrap();
        let backup_dir = std::env::temp_dir().join(format!(
            "membrick-legacy-adoption-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let adoption =
            adopt_legacy_extension_records(&mut service, "mcp", workspace, &backup_dir).unwrap();
        assert_eq!(adoption.adopted, 2);
        assert_eq!(adoption.left_in_place, 1);
        let backup = std::fs::read_to_string(adoption.backup.unwrap()).unwrap();
        assert_eq!(backup.lines().count(), 2);

        {
            let store = McpExtensionDocuments {
                service: Rc::new(RefCell::new(&mut service)),
                owner: "mcp".to_string(),
                workspace: workspace.to_string(),
                extension_id: LEGACY_NOTE_EXTENSION.to_string(),
            };
            let adopted = store.get(&first.doc_id).unwrap().unwrap();
            assert_eq!(adopted.kind, "note");
            assert_eq!(adopted.value, json!({"text": "First step done."}));
        }

        let remaining = service
            .source_documents()
            .into_iter()
            .map(|document| document.doc_id.clone())
            .collect::<Vec<_>>();
        assert!(!remaining.contains(&first.doc_id));
        assert!(!remaining.contains(&second.doc_id));
        assert!(remaining.contains(&foreign.doc_id));

        let again =
            adopt_legacy_extension_records(&mut service, "mcp", workspace, &backup_dir).unwrap();
        assert_eq!(again.adopted, 0);
        assert!(again.backup.is_none());
        let _ = std::fs::remove_dir_all(backup_dir);
    }

    #[test]
    fn provider_filter_restricts_search_to_one_providers_memories() {
        use crate::query_plan::PreparedQuery;

        fn memory(doc_id: &str, provider: &str) -> SourceDocument {
            SourceDocument {
                doc_id: doc_id.to_string(),
                source: format!("{provider}://project/session/outcome"),
                content: "The deployment pipeline codename is cobalt".into(),
                concept: "outcome".to_string(),
                group_id: Some(format!("{provider}-session:session")),
                filters: BTreeMap::from([("provider".to_string(), provider.to_string())]),
                headings: vec![],
                links: vec![],
                timestamp: None,
                doc_length: 38,
                author_agent: Some(provider.to_string()),
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            }
        }

        let mut store = IndexStore::in_memory(PipelineOptions::default());
        store.upsert(memory("codex-memory", "codex"));
        store.upsert(memory("claude-memory", "claude"));
        store.refresh().unwrap();

        // Unfiltered search sees the shared pool.
        let unfiltered = store
            .query_prepared(
                &PreparedQuery::new("deployment pipeline codename"),
                10,
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(unfiltered.len(), 2);

        // A provider filter sees only that provider's memories.
        let filters = search_provider_filters(&json!({"query": "x", "provider": "codex"})).unwrap();
        let filtered = store
            .query_prepared(
                &PreparedQuery::new("deployment pipeline codename"),
                10,
                &filters,
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].doc_id, "codex-memory");
    }

    #[test]
    fn list_memories_is_bounded_and_provider_neutral() {
        let mut store = IndexStore::in_memory(PipelineOptions::default());
        store.upsert(SourceDocument {
            doc_id: "memory-1".to_string(),
            source: "codex://project/session-1/outcome".to_string(),
            content: "memory content".into(),
            concept: "decision".to_string(),
            group_id: Some("session-1".to_string()),
            filters: BTreeMap::new(),
            headings: vec![],
            links: vec![],
            timestamp: None,
            doc_length: 14,
            author_agent: None,
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        });
        store.upsert(SourceDocument {
            doc_id: "workspace-file".to_string(),
            source: "file:///workspace/README.md".to_string(),
            content: "workspace content".into(),
            concept: "source-file".to_string(),
            group_id: None,
            filters: BTreeMap::new(),
            headings: vec![],
            links: vec![],
            timestamp: None,
            doc_length: 17,
            author_agent: None,
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        });
        let service = MemoryService::new(store);
        let payload = list_memories(&service, 20);
        assert_eq!(payload["count"], 1);
        assert_eq!(
            payload["memories"][0]["source"],
            "codex://project/session-1/outcome"
        );
        assert_eq!(parse_list_memories_limit(&json!({"limit": 0})).unwrap(), 1);
        assert!(parse_list_memories_limit(&json!({"unexpected": true})).is_err());
    }

    #[test]
    fn list_memories_recognizes_canonical_claude_code_sources() {
        let document = SourceDocument {
            doc_id: "claude-session".to_string(),
            source: "claude-code://project/session/outcome".to_string(),
            content: "captured Claude session".into(),
            concept: "outcome".to_string(),
            group_id: None,
            headings: vec![],
            links: vec![],
            timestamp: None,
            doc_length: 23,
            author_agent: Some("claude".to_string()),
            filters: BTreeMap::new(),
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        };

        assert!(is_recorded_memory(&document));
    }

    #[test]
    fn list_memories_recognizes_muse_session_tag() {
        let document = SourceDocument {
            doc_id: "muse-session".to_string(),
            source: "muse://project/session/outcome".to_string(),
            content: "captured Muse session".into(),
            concept: "outcome".to_string(),
            group_id: Some("muse-session:project:session".to_string()),
            headings: vec![],
            links: vec![],
            timestamp: None,
            doc_length: 21,
            author_agent: Some("muse".to_string()),
            filters: BTreeMap::new(),
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        };

        assert!(is_recorded_memory(&document));
    }

    #[test]
    fn search_excerpt_keeps_a_late_query_match_in_a_long_capture() {
        let content = format!("{} late-evidence-913", "prefix ".repeat(2_000));
        let query = "late-evidence-913";
        let initial = crate::integrations::recall::relevant_excerpt(&content, query, &[], 4_000);
        assert!(!initial.contains(query));

        let excerpt = preserve_matched_excerpt(&content, &initial, Some(query), &[], 4_000);
        assert!(excerpt.contains(query));
        assert!(excerpt.len() <= 4_000);
    }

    #[test]
    fn comparison_search_reports_uncovered_operands_when_empty() {
        let service = MemoryService::in_memory(PipelineOptions::default());
        let payload = format_search_results(
            &service,
            Some("Compare old and new command interface"),
            Vec::new(),
        );

        assert_eq!(payload["results"], json!([]));
        assert_eq!(payload["comparison"]["covered_sides"], 0);
        assert_eq!(payload["comparison"]["total_sides"], 2);
        assert_eq!(payload["comparison"]["sides"][0]["covered"], false);
        assert_eq!(payload["comparison"]["sides"][1]["covered"], false);
    }

    #[test]
    fn search_results_anchor_memories_to_their_session_date() {
        fn memory(doc_id: &str, timestamp: Option<&str>) -> SourceDocument {
            SourceDocument {
                doc_id: doc_id.to_string(),
                source: "codex://project/session-1/outcome".to_string(),
                content: "we deployed yesterday".into(),
                concept: "outcome".to_string(),
                group_id: Some("codex-session:project:session-1".to_string()),
                filters: BTreeMap::new(),
                headings: vec![],
                links: vec![],
                timestamp: timestamp.map(str::to_string),
                doc_length: 21,
                author_agent: None,
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            }
        }
        fn hit(doc_id: &str) -> SearchResult {
            SearchResult {
                doc_id: doc_id.to_string(),
                source: "codex://project/session-1/outcome".to_string(),
                group_id: Some("codex-session:project:session-1".to_string()),
                score: 1.0,
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

        let mut store = IndexStore::in_memory(PipelineOptions::default());
        store.upsert(memory("dated", Some("2026-09-20T10:00:00Z")));
        store.upsert(memory("undated", None));
        let mut file = memory("file-modified", Some("1792220974"));
        file.source = "docs/guide.md".to_string();
        file.group_id = None;
        file.filters
            .insert("timestamp_kind".to_string(), "source_modified".to_string());
        file.content = "Repository guide contents".into();
        store.upsert(file);
        let mut event = memory("file-event", Some("1792220974000"));
        event.source = "membrick://workspace/file-change/abc".to_string();
        event.group_id = Some("workspace-file-changes".to_string());
        event.filters.insert(
            "source_type".to_string(),
            "workspace-file-change".to_string(),
        );
        event
            .filters
            .insert("timestamp_kind".to_string(), "workspace_event".to_string());
        event.content = "Workspace file change".into();
        store.upsert(event);
        let service = MemoryService::new(store);
        let payload = search_results(
            &service,
            vec![
                hit("dated"),
                hit("undated"),
                hit("file-modified"),
                hit("file-event"),
            ],
        );
        let results = payload["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["doc_id"], "dated");
        assert_eq!(results[0]["rank"], 1);
        assert_eq!(results[1]["doc_id"], "undated");
        assert_eq!(results[1]["rank"], 2);

        let dated = &results[0];
        assert_eq!(dated["created_at"], "2026-09-20T10:00:00Z");
        assert_eq!(dated["timestamp_kind"], "session");
        assert_eq!(dated["session_id"], "codex-session:project:session-1");
        let content = dated["content"].as_str().unwrap();
        let indexed_dated = service.source_document_by_id("dated").unwrap();
        assert_eq!(
            dated["source_hash"],
            crate::source::source_document_hash(&indexed_dated).unwrap()
        );
        assert!(
            content.starts_with("[session date: 2026-09-20T10:00:00Z]\n"),
            "date prefix missing: {content}"
        );
        assert!(content.ends_with("we deployed yesterday"));

        // Fail-open: a memory without a timestamp keeps its original text.
        let undated = &results[1];
        assert!(undated["created_at"].is_null());
        assert_eq!(undated["content"], "we deployed yesterday");

        let file_modified = &results[2];
        assert_eq!(file_modified["timestamp_kind"], "source_modified");
        assert!(file_modified["created_at"].is_null());
        assert_eq!(file_modified["modified_at"], "2026-10-17T07:09:34Z");
        assert_eq!(file_modified["content"], "Repository guide contents");
        assert!(!file_modified["content"]
            .as_str()
            .unwrap()
            .contains("session date"));

        let file_event = &results[3];
        assert_eq!(file_event["timestamp_kind"], "workspace_event");
        assert!(file_event["created_at"].is_null());
        assert_eq!(file_event["event_at"], "2026-10-17T07:09:34Z");
        assert_eq!(file_event["content"], "Workspace file change");
    }

    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code"
    ))]
    #[test]
    fn resolve_search_session_id_prefers_explicit_over_pointer() {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
                "membrick-resolve-session-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0),
            ));
        let service =
            MemoryService::at_path(&dir, PipelineOptions::default()).expect("service opens");
        service.note_active_session("claude", "hook-session");
        // Explicit argument wins over the hook-written pointer...
        let resolved = resolve_search_session_id(
            &json!({"query": "q", "session_id": "agent-session"}),
            &service,
            "claude",
        )
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("agent-session"));
        // Explicit request routing does not change the lifecycle active set.
        assert_eq!(
            service.current_session_id("claude").as_deref(),
            Some("hook-session")
        );
        // Absent argument falls back to the pointer.
        let resolved =
            resolve_search_session_id(&json!({"query": "q"}), &service, "claude").unwrap();
        assert_eq!(resolved.as_deref(), Some("hook-session"));
        // A present-but-blank argument is still rejected, not silently
        // degraded to the pointer.
        assert!(resolve_search_session_id(
            &json!({"query": "q", "session_id": "  "}),
            &service,
            "claude"
        )
        .is_err());
        // No argument and no pointer means stateless.
        let fresh = MemoryService::at_path(&dir.join("fresh"), PipelineOptions::default())
            .expect("service opens");
        let resolved = resolve_search_session_id(&json!({"query": "q"}), &fresh, "claude").unwrap();
        assert_eq!(resolved, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
                "membrick-{}-{}-{}",
                name,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0),
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Unwrap a successful `tools/call` envelope back to its JSON payload.
    fn tool_payload(response: &JsonRpcResponse) -> Value {
        assert!(response.error.is_none(), "tool error: {:?}", response.error);
        let text = response.result.as_ref().expect("result")["content"][0]["text"]
            .as_str()
            .expect("text content");
        serde_json::from_str(text).expect("payload parses")
    }

    /// P1: board and memory writes must land on the persistent shared store,
    /// not the in-memory composed view. Drives `board_open` -> `board_post`
    /// and `add_memory` through the shared dispatch, drops the view entirely
    /// (simulating process exit), then reopens the shared store fresh and
    /// requires the board, post, and memory to be present and readable.
    #[cfg(feature = "flowboard-tests")]
    #[test]
    fn board_and_memory_writes_survive_reopen() {
        use crate::integrations::mcp_index;
        let root = test_root("board-persist");
        // Build the composed in-memory view exactly like the adapters do.
        let mut view = crate::memory_api::MemoryService::open_workspace(
            &root,
            mcp_index::SHARED_MEMORY_DIR,
            &[],
            || Ok(vec![]),
        )
        .expect("view opens");
        let workspace = root.to_string_lossy().to_string();

        let opened = tool_payload(
            &call_mcp_tool(
                "board_open",
                None,
                &json!({"key": "k1", "title": "Board", "session_id": "sess-p1"}),
                &mut view,
                &root,
                "openclaw",
            )
            .expect("board_open dispatches"),
        );
        let board_id = opened["board_id"]
            .as_str()
            .expect("board_id in payload")
            .to_string();

        let posted = tool_payload(
            &call_mcp_tool(
                "board_post",
                None,
                &json!({"board_id": board_id, "content": "hello board",
                        "request_id": "post-1", "session_id": "sess-p1"}),
                &mut view,
                &root,
                "openclaw",
            )
            .expect("board_post dispatches"),
        );
        assert_eq!(posted["content"], json!("hello board"));

        let added = tool_payload(
            &call_mcp_tool(
                "add_memory",
                None,
                &json!({"content": "The API rate limit is 100 requests per minute.",
                        "request_id": "mem-p1", "session_id": "sess-p1"}),
                &mut view,
                &root,
                "openclaw",
            )
            .expect("add_memory dispatches"),
        );
        assert_eq!(added["success"], json!(true));

        // Simulate process exit: the in-memory view is gone.
        drop(view);

        // Reopen the persistent shared store fresh, as another process would.
        let mut store = MemoryService::at_path(
            mcp_index::shared_memory_root(&root),
            mcp_index::segmented_store_options(),
        )
        .expect("shared store reopens");

        let boards = dispatch_extension_tool(
            "board_list",
            &json!({"session_id": "sess-p1"}),
            &mut store,
            "mcp",
            &workspace,
            "openclaw",
        )
        .expect("board_list");
        assert!(
            boards["boards"]
                .as_array()
                .unwrap()
                .iter()
                .any(|board| board["board_id"] == board_id),
            "board missing after reopen: {boards}"
        );
        let posts = dispatch_extension_tool(
            "board_read",
            &json!({"board_id": board_id, "session_id": "sess-p1"}),
            &mut store,
            "mcp",
            &workspace,
            "openclaw",
        )
        .expect("board_read");
        assert!(
            posts["posts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|post| post["content"] == "hello board"),
            "post missing after reopen: {posts}"
        );
        let memory_id = crate::memory_api::memory_document_id("mcp", "mem-p1", 0);
        let record = dispatch_memory_tool(
            "get_memory",
            &json!({"memory_id": memory_id}),
            &mut store,
            "openclaw",
        )
        .expect("get_memory");
        assert!(
            record["content"].as_str().unwrap().contains("rate limit"),
            "memory missing after reopen: {record}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
