use super::{decode, ensure_success, Connection, Memory, MemoryError};
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
    pub results: Vec<Value>,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}

impl Memory {
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
