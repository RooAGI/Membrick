use super::{decode, ensure_success, Connection, Memory, MemoryError};
use crate::memory_api::{ReviewSearchResult, SearchReviewResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Provider and workspace identity for workspace-scoped agent operations.
/// Project clients are pinned to their configured root. Remote clients must
/// provide an explicit workspace on every operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOperationContext {
    pub workspace: Option<String>,
    pub provider: String,
    pub session_id: Option<String>,
}

impl AgentOperationContext {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            workspace: None,
            provider: provider.into(),
            session_id: None,
        }
    }

    fn resolve_workspace(&self, client: &Memory) -> Result<String, MemoryError> {
        if self.provider.trim().is_empty() {
            return Err(MemoryError::Configuration {
                message: "provider must not be empty".into(),
            });
        }
        match &client.connection {
            Connection::Project { root, .. } => {
                if let Some(requested) = self.workspace.as_deref() {
                    let requested = std::fs::canonicalize(requested).map_err(|error| {
                        MemoryError::Configuration {
                            message: format!("invalid workspace: {error}"),
                        }
                    })?;
                    if requested != *root {
                        return Err(MemoryError::Configuration {
                            message: "workspace does not match the project's resident root".into(),
                        });
                    }
                }
                Ok(root.to_string_lossy().into_owned())
            }
            Connection::Remote { .. } => self
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|workspace| !workspace.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| MemoryError::Configuration {
                    message: "remote workspace operations require an explicit workspace".into(),
                }),
        }
    }
}

/// Host event payload submitted to the resident capture queue. The client
/// preserves `input` verbatim; provider-specific parsing stays in adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub document_type: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub provider: String,
    pub session_id: String,
    pub captured_bytes: u64,
    pub target_bytes: u64,
    pub remaining_bytes: u64,
    pub status: String,
    pub last_progress_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReceipt {
    pub accepted: bool,
    pub capture: CaptureStatus,
}

/// Search response returned by the provider's workspace search operation.
/// Result values retain the server's documented agent fields so adapters can
/// format host-specific context without reimplementing search policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSearchResponse {
    /// ID accepted by `review_search` for this exact workspace search.
    pub search_id: String,
    pub results: Vec<Value>,
    /// Present when the query has two comparison operands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Value>,
    /// Guidance for evaluating these results with the workspace review tools.
    pub feedback_guidance: String,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}

/// Select one recorded workspace search or page through unreviewed searches.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewSearchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<usize>,
}

/// Select which Membrick search history to review.
#[derive(Debug, Clone)]
pub enum ReviewSearchTarget {
    /// Recorded search made against indexed workspace content.
    Workspace(AgentOperationContext),
    /// Search made through the user-scoped `Memory::search` API.
    User { user_id: String },
}

impl ReviewSearchRequest {
    /// Retrieve the recorded query, results, and available source documents.
    pub fn by_id(search_id: impl Into<String>) -> Self {
        Self {
            search_id: Some(search_id.into()),
            ..Self::default()
        }
    }

    /// Page through searches that have not yet been reviewed.
    pub fn unreviewed(cursor: Option<String>, page_size: Option<usize>) -> Self {
        Self {
            cursor,
            page_size,
            ..Self::default()
        }
    }
}

impl Memory {
    /// Retrieve the recorded details and source documents for a user-scoped
    /// memory search. The response is the same DTO exposed by the Python and
    /// Node.js clients.
    pub fn review_memory_search(
        &self,
        search_id: &str,
        user_id: &str,
    ) -> Result<SearchReviewResponse, MemoryError> {
        if search_id.trim().is_empty() {
            return Err(MemoryError::Configuration {
                message: "review search_id must not be empty".into(),
            });
        }
        if user_id.trim().is_empty() {
            return Err(MemoryError::Configuration {
                message: "review user_id must not be empty".into(),
            });
        }
        let payload = json!({"search_id": search_id, "user_id": user_id});
        let (status, body) =
            self.request("POST", "/v1/memories/search/review", Some(&payload), false)?;
        ensure_success(status, &body)?;
        decode(body)
    }

    /// Queue a provider transcript for durable background capture. A receipt
    /// means accepted by the queue, not yet persisted/searchable.
    pub fn submit_capture(
        &self,
        context: &AgentOperationContext,
        request: CaptureRequest,
    ) -> Result<CaptureReceipt, MemoryError> {
        let workspace = context.resolve_workspace(self)?;
        let session_id = request
            .input
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| MemoryError::Configuration {
                message: "capture input must preserve a non-empty host session_id".into(),
            })?;
        if context
            .session_id
            .as_deref()
            .is_some_and(|context_session| context_session != session_id)
        {
            return Err(MemoryError::Configuration {
                message: "capture input session_id does not match the operation context".into(),
            });
        }
        let payload = json!({
            "workspace": workspace,
            "provider": context.provider.as_str(),
            "document_type": request.document_type,
            "input": request.input,
        });
        let (status, body) =
            self.request("POST", "/provider-memory/captures", Some(&payload), true)?;
        if status != 202 {
            ensure_success(status, &body)?;
            return Err(MemoryError::Rejected {
                status,
                message: "capture endpoint did not acknowledge queue acceptance".into(),
            });
        }
        let receipt: CaptureReceipt = decode(body)?;
        // The server is authoritative; assert that its acknowledgement still
        // refers to the original host session and selected provider.
        if receipt.capture.provider != context.provider || receipt.capture.session_id != session_id
        {
            return Err(MemoryError::Uncertain {
                message:
                    "capture acknowledgement identity did not match the submitted host session"
                        .into(),
            });
        }
        Ok(receipt)
    }

    /// Search the shared captured-memory and workspace-memory index.
    pub fn workspace_search(
        &self,
        context: &AgentOperationContext,
        query: &str,
        top_k: usize,
    ) -> Result<WorkspaceSearchResponse, MemoryError> {
        if query.trim().is_empty() {
            return Err(MemoryError::Configuration {
                message: "workspace search query must not be empty".into(),
            });
        }
        let arguments = json!({
            "query": query,
            "top_k": top_k,
            "session_id": context.session_id,
        });
        let body = self.workspace_tool(context, "search", arguments, false)?;
        decode(body)
    }

    /// Review a recorded workspace or user-scoped search.
    ///
    /// Exact review returns the recorded query and results together with
    /// available indexed source documents. Workspace searches can also page
    /// through unreviewed searches. User-scoped memory searches must be
    /// reviewed by exact `search_id` and matching `user_id`.
    pub fn review_search(
        &self,
        target: ReviewSearchTarget,
        request: ReviewSearchRequest,
    ) -> Result<ReviewSearchResult, MemoryError> {
        if request
            .search_id
            .as_deref()
            .is_some_and(|search_id| search_id.trim().is_empty())
        {
            return Err(MemoryError::Configuration {
                message: "review search_id must not be empty".into(),
            });
        }
        match target {
            ReviewSearchTarget::Workspace(context) => {
                let arguments =
                    serde_json::to_value(request).map_err(|error| MemoryError::Configuration {
                        message: format!("invalid review search request: {error}"),
                    })?;
                decode(self.workspace_tool(&context, "review_search", arguments, false)?)
            }
            ReviewSearchTarget::User { user_id } => {
                if request.search_id.is_none()
                    || request.cursor.is_some()
                    || request.page_size.is_some()
                {
                    return Err(MemoryError::Configuration {
                        message: "user search review requires only an exact search_id".into(),
                    });
                }
                let search_id = request.search_id.expect("validated above");
                let response = self.review_memory_search(&search_id, &user_id)?;
                Ok(ReviewSearchResult::Exact(response))
            }
        }
    }

    /// Durably publish adapter-parsed capture documents through the resident
    /// writer. Adapters retain transcript parsing and advance cursors only
    /// after this method acknowledges persistence.
    pub fn publish_capture_documents(
        &self,
        context: &AgentOperationContext,
        session_id: &str,
        documents: Vec<crate::SourceDocument>,
        replace_group: Option<(String, String)>,
    ) -> Result<(), MemoryError> {
        let workspace = context.resolve_workspace(self)?;
        if session_id.trim().is_empty() {
            return Err(MemoryError::Configuration {
                message: "capture session_id must not be empty".into(),
            });
        }
        if context
            .session_id
            .as_deref()
            .is_some_and(|value| value != session_id)
        {
            return Err(MemoryError::Configuration {
                message: "capture session_id does not match the operation context".into(),
            });
        }
        let mut payload = json!({
            "workspace": workspace,
            "provider": context.provider,
            "session_id": session_id,
            "user_id": crate::MCP_MEMORY_USER_ID,
            "documents": documents,
        });
        if let Some((key, value)) = replace_group {
            payload["replace_group"] = json!({"key": key, "value": value});
        }
        let (status, body) = self.request(
            "POST",
            crate::MCP_DOCUMENTS_UPSERT_PATH,
            Some(&payload),
            true,
        )?;
        ensure_success(status, &body)
    }

    /// Dispatch a generic resident-owned MCP workspace tool through the same
    /// classified transport used by the typed SDK operations. Mutations are
    /// never replayed after dispatch may have begun.
    pub(crate) fn call_workspace_tool(
        &self,
        context: &AgentOperationContext,
        tool: &str,
        arguments: Value,
        may_mutate: bool,
    ) -> Result<Value, MemoryError> {
        self.call_workspace_tool_with_transport(context, tool, arguments, may_mutate)
            .map(|(body, _)| body)
    }

    pub(crate) fn call_workspace_tool_with_transport(
        &self,
        context: &AgentOperationContext,
        tool: &str,
        arguments: Value,
        may_mutate: bool,
    ) -> Result<(Value, &'static str), MemoryError> {
        if !arguments.is_object() {
            return Err(MemoryError::Configuration {
                message: "workspace tool arguments must be a JSON object".into(),
            });
        }
        let workspace = context.resolve_workspace(self)?;
        let payload = json!({
            "workspace": workspace,
            "provider": context.provider.as_str(),
            "user_id": crate::MCP_MEMORY_USER_ID,
            "session_id": context.session_id,
            "tool": tool,
            "arguments": arguments,
        });
        let (status, body, transport) = self.request_with_transport(
            "POST",
            crate::MCP_TOOLS_CALL_PATH,
            Some(&payload),
            may_mutate,
        )?;
        ensure_success(status, &body)?;
        Ok((body, transport))
    }

    fn workspace_tool(
        &self,
        context: &AgentOperationContext,
        tool: &str,
        arguments: Value,
        may_mutate: bool,
    ) -> Result<Value, MemoryError> {
        self.call_workspace_tool(context, tool, arguments, may_mutate)
    }
}
