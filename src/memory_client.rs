//! Shared application-facing connection and operation API.
//!
//! The resident/server owns `MemoryService`; this client only prepares typed
//! requests and selects the local IPC or remote HTTP transport.

use crate::memory_api::{
    AddRequest, AddRequestWithMetadata, AddResponse, ListResponse, Mem0FilterExpr, MemoryRecord,
    Message, SearchMemory, SearchRequest, SearchRequestWithFilterAst, SearchResponse,
    UpdateRequest, UpdateRequestWithMetadata,
};
use reqwest::Client as AsyncHttpClient;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod agent_operations;
pub use agent_operations::{
    AgentOperationContext, CaptureReceipt, CaptureRequest, CaptureStatus, WorkspaceSearchResponse,
};

/// Connection and request settings shared by Rust and language bindings.
#[derive(Clone)]
pub struct MemoryOptions {
    pub request_timeout: Duration,
    pub api_key: Option<String>,
    /// Explicit override for the CLI used to start a project resident.
    /// Without an override, bindings may provide their bundled CLI and the
    /// shared resolver searches PATH for `membrick`.
    pub resident_executable: Option<PathBuf>,
}

impl std::fmt::Debug for MemoryOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryOptions")
            .field("request_timeout", &self.request_timeout)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("resident_executable", &self.resident_executable)
            .finish()
    }
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            api_key: None,
            resident_executable: None,
        }
    }
}

/// Definitive rejection, uncertain dispatch outcome, unavailable service, or
/// local client/response configuration failure.
#[derive(Debug)]
pub enum MemoryError {
    Rejected { status: u16, message: String },
    Uncertain { message: String },
    Unavailable { message: String },
    Configuration { message: String },
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { status, message } => {
                write!(f, "memory request rejected ({status}): {message}")
            }
            Self::Uncertain { message } => write!(f, "memory request outcome uncertain: {message}"),
            Self::Unavailable { message } => write!(f, "memory service unavailable: {message}"),
            Self::Configuration { message } => {
                write!(f, "invalid memory client configuration: {message}")
            }
        }
    }
}
impl std::error::Error for MemoryError {}

/// Backward-compatible alias. Prefer [`MemoryOptions`].
#[deprecated(note = "use MemoryOptions")]
pub type MemoryClientOptions = MemoryOptions;

/// Backward-compatible alias. Prefer [`MemoryError`].
#[deprecated(note = "use MemoryError")]
pub type MemoryClientError = MemoryError;

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Caller-managed durable idempotency key. Generated once when absent.
    pub request_id: Option<String>,
    /// Explicit conversation identity. If absent, add creates a standalone ID.
    pub session_id: Option<String>,
    pub wait_for_visibility: bool,
}

#[derive(Serialize)]
struct AddRequestWire<'a> {
    request_id: &'a str,
    messages: &'a [Message],
    user_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    metadata: &'a std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub top_k: usize,
    pub options: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub scope: Option<String>,
    pub filters: Option<std::collections::BTreeMap<String, String>>,
    pub reference_date: Option<String>,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            top_k: 10,
            options: None,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GetOptions {
    pub include_inactive: bool,
}
#[derive(Debug, Clone)]
pub struct ListOptions {
    pub session_id: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
    pub include_inactive: bool,
}
impl Default for ListOptions {
    fn default() -> Self {
        Self {
            session_id: None,
            limit: 100,
            cursor: None,
            include_inactive: false,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub role: Option<String>,
    pub timestamp: Option<i64>,
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone)]
enum Connection {
    Project {
        root: PathBuf,
        resident_executable: Option<PathBuf>,
    },
    Remote {
        base_url: String,
        async_client: AsyncHttpClient,
        api_key: Option<String>,
    },
}

/// Synchronous SDK connection. This does not own `MemoryService`.
pub struct Memory {
    connection: Connection,
    session_lease: Mutex<Option<Arc<crate::local_ipc::ProviderSessionLease>>>,
}

impl Clone for Memory {
    fn clone(&self) -> Self {
        let session_lease = self
            .session_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Self {
            connection: self.connection.clone(),
            session_lease: Mutex::new(session_lease),
        }
    }
}

/// Explicit, restorable conversation context. Construct with `session` using
/// the saved IDs returned by add; the value never guesses a previous session.
pub struct MemorySession {
    client: Memory,
    user_id: String,
    session_id: String,
}

impl MemorySession {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// Release this session handle's resident lease without closing other
    /// client or session handles.
    pub fn close(&self) {
        self.client.close();
    }
    pub fn add(
        &self,
        messages: Vec<Message>,
        mut options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        options.session_id = Some(self.session_id.clone());
        self.client.add(messages, &self.user_id, options)
    }
    pub fn search(
        &self,
        query: &str,
        mut options: SearchOptions,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        options.session_id = Some(self.session_id.clone());
        self.client.search(query, &self.user_id, options)
    }
    pub async fn add_async(
        &self,
        messages: Vec<Message>,
        mut options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        options.session_id = Some(self.session_id.clone());
        self.client
            .add_async(messages, &self.user_id, options)
            .await
    }
    pub async fn search_async(
        &self,
        query: &str,
        mut options: SearchOptions,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        options.session_id = Some(self.session_id.clone());
        self.client
            .search_async(query, &self.user_id, options)
            .await
    }
}

impl Memory {
    /// Connect to the resident for an existing project root (the directory
    /// containing `.membrick`). This never initializes project files. This
    /// synchronous constructor performs filesystem and lease operations and
    /// can block; async applications should use [`Memory::new_async`].
    pub fn new(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        Self::with_options(path, MemoryOptions::default())
    }

    /// Connect to a project using explicit resident launch options.
    pub fn with_options(
        path: impl AsRef<Path>,
        options: MemoryOptions,
    ) -> Result<Self, MemoryError> {
        let path = path.as_ref();
        let settings = path.join(".membrick").join("server.toml");
        if !settings.is_file() {
            return Err(config(format!(
                "not a Membrick project: {} (initialize it explicitly with Memory::init)",
                path.display()
            )));
        }
        let root = crate::state_paths::validate_project_root(path).map_err(config_error)?;
        let session_lease =
            crate::local_ipc::acquire_provider_session_lease(&root, "memory-client")
                .map(Arc::new)
                .map_err(|error| MemoryError::Unavailable {
                    message: format!("could not hold a project resident lease: {error:#}"),
                })?;
        Ok(Self {
            connection: Connection::Project {
                root,
                resident_executable: options.resident_executable,
            },
            session_lease: Mutex::new(Some(session_lease)),
        })
    }

    /// Connect to an existing project without blocking the calling executor.
    /// Filesystem validation and persistent lease acquisition run on Tokio's
    /// blocking pool. This never initializes project files.
    pub async fn new_async(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        Self::with_options_async(path, MemoryOptions::default()).await
    }

    /// Async variant of [`Memory::with_options`].
    pub async fn with_options_async(
        path: impl AsRef<Path>,
        options: MemoryOptions,
    ) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_owned();
        let (root, session_lease) = tokio::task::spawn_blocking(move || {
            let settings = path.join(".membrick").join("server.toml");
            if !settings.is_file() {
                return Err(config(format!(
                    "not a Membrick project: {} (initialize it explicitly with Memory::init)",
                    path.display()
                )));
            }
            let root = crate::state_paths::validate_project_root(&path).map_err(config_error)?;
            let lease = crate::local_ipc::acquire_provider_session_lease(&root, "memory-client")
                .map(Arc::new)
                .map_err(|error| MemoryError::Unavailable {
                    message: format!("could not hold a project resident lease: {error:#}"),
                })?;
            Ok((root, lease))
        })
        .await
        .map_err(|error| MemoryError::Unavailable {
            message: format!("project client initialization task failed: {error}"),
        })??;

        Ok(Self {
            connection: Connection::Project {
                root,
                resident_executable: options.resident_executable,
            },
            session_lease: Mutex::new(Some(session_lease)),
        })
    }

    /// Initialize a Membrick project at `path`, then connect to its resident.
    /// This is the only constructor that writes project configuration files.
    pub fn init(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref();
        crate::state_paths::initialize_project_state(path).map_err(config_error)?;
        Self::new(path)
    }

    /// Initialize project state and connect without blocking the calling
    /// executor. Use [`Memory::new_async`] when the project already exists.
    pub async fn init_async(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_owned();
        let init_path = path.clone();
        tokio::task::spawn_blocking(move || {
            crate::state_paths::initialize_project_state(&init_path).map_err(config_error)
        })
        .await
        .map_err(|error| MemoryError::Unavailable {
            message: format!("project initialization task failed: {error}"),
        })??;
        Self::new_async(path).await
    }

    /// Initialize a project and connect with explicit resident launch options.
    pub fn init_with_options(
        path: impl AsRef<Path>,
        options: MemoryOptions,
    ) -> Result<Self, MemoryError> {
        crate::state_paths::initialize_project_state(path.as_ref()).map_err(config_error)?;
        Self::with_options(path, options)
    }

    /// Initialize project state and connect with explicit options without
    /// blocking the calling executor.
    pub async fn init_with_options_async(
        path: impl AsRef<Path>,
        options: MemoryOptions,
    ) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_owned();
        let init_path = path.clone();
        tokio::task::spawn_blocking(move || {
            crate::state_paths::initialize_project_state(&init_path).map_err(config_error)
        })
        .await
        .map_err(|error| MemoryError::Unavailable {
            message: format!("project initialization task failed: {error}"),
        })??;
        Self::with_options_async(path, options).await
    }

    pub fn remote(
        base_url: impl Into<String>,
        options: MemoryOptions,
    ) -> Result<Self, MemoryError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let parsed = reqwest::Url::parse(&base_url).map_err(config_error)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(config("base_url must be an absolute http or https URL"));
        }
        let async_client = AsyncHttpClient::builder()
            .timeout(options.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(config_error)?;
        Ok(Self {
            connection: Connection::Remote {
                base_url,
                async_client,
                api_key: options.api_key,
            },
            session_lease: Mutex::new(None),
        })
    }

    /// Release this handle's resident lease. This is idempotent and does not
    /// disable calls: each project request still holds a temporary lease while
    /// it runs. Dropping the last client or session handle releases the lease.
    pub fn close(&self) {
        self.session_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Restore a user-scoped session from persisted IDs without creating a new one.
    pub fn session(
        &self,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> MemorySession {
        MemorySession {
            client: self.clone(),
            user_id: user_id.into(),
            session_id: session_id.into(),
        }
    }

    pub fn add(
        &self,
        messages: Vec<Message>,
        user_id: &str,
        options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        self.add_with_metadata(
            messages,
            user_id,
            std::collections::BTreeMap::new(),
            options,
        )
    }

    pub fn add_with_metadata(
        &self,
        messages: Vec<Message>,
        user_id: &str,
        metadata: std::collections::BTreeMap<String, serde_json::Value>,
        options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        let client = self.clone();
        let user_id = user_id.to_owned();
        run_sync(async move {
            client
                .add_async_with_metadata(messages, &user_id, metadata, options)
                .await
        })
    }

    /// Add messages without blocking the calling executor. The request ID and
    /// resolved session ID are created before the first transport attempt and
    /// remain unchanged across safe retries.
    pub async fn add_async(
        &self,
        messages: Vec<Message>,
        user_id: &str,
        options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        self.add_async_with_metadata(
            messages,
            user_id,
            std::collections::BTreeMap::new(),
            options,
        )
        .await
    }

    pub async fn add_async_with_metadata(
        &self,
        messages: Vec<Message>,
        user_id: &str,
        metadata: std::collections::BTreeMap<String, serde_json::Value>,
        options: AddOptions,
    ) -> Result<AddResponse, MemoryError> {
        let explicit_request_id = options.request_id.is_some();
        let request_id = options.request_id.unwrap_or_else(generated_id);
        let session_id_was_omitted = options.session_id.is_none();
        let session_id = options.session_id.unwrap_or_else(|| {
            if explicit_request_id {
                retry_session_id(user_id, &request_id)
            } else {
                generated_id()
            }
        });
        let request = AddRequest {
            request_id,
            messages,
            user_id: user_id.to_owned(),
            session_id,
        };
        let path = if options.wait_for_visibility {
            "/v1/memories?wait_for_visibility=true"
        } else {
            "/v1/memories"
        };
        let wrapped = AddRequestWithMetadata { request, metadata };
        let wire_omits_session_id = explicit_request_id && session_id_was_omitted;
        let wire = AddRequestWire {
            request_id: &wrapped.request.request_id,
            messages: &wrapped.request.messages,
            user_id: &wrapped.request.user_id,
            session_id: (!wire_omits_session_id).then_some(wrapped.request.session_id.as_str()),
            metadata: &wrapped.metadata,
        };
        let value = self.post_async(path, &wire, true).await?;
        decode(value)
    }

    pub fn search(
        &self,
        query: &str,
        user_id: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        let client = self.clone();
        let query = query.to_owned();
        let user_id = user_id.to_owned();
        run_sync(async move { client.search_async(&query, &user_id, options).await })
    }

    pub async fn search_async(
        &self,
        query_text: &str,
        user_id: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        self.search_with_filter_ast_async(query_text, user_id, options, None)
            .await
    }

    /// Search with a typed Mem0 Platform filter AST. Ownership remains bound
    /// to `user_id` independently of the caller-provided expression.
    pub fn search_with_filter_ast(
        &self,
        query: &str,
        user_id: &str,
        options: SearchOptions,
        filter_ast: Mem0FilterExpr,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        let client = self.clone();
        let query = query.to_owned();
        let user_id = user_id.to_owned();
        run_sync(async move {
            client
                .search_with_filter_ast_async(&query, &user_id, options, Some(filter_ast))
                .await
        })
    }

    pub async fn search_with_filter_ast_async(
        &self,
        query_text: &str,
        user_id: &str,
        options: SearchOptions,
        filter_ast: Option<Mem0FilterExpr>,
    ) -> Result<Vec<SearchMemory>, MemoryError> {
        let request = SearchRequest {
            query: query_text.to_owned(),
            options: options.options,
            user_id: user_id.to_owned(),
            top_k: options.top_k,
            session_id: options.session_id,
            scope: options.scope,
            filters: options.filters,
            reference_date: options.reference_date,
        };
        let request = SearchRequestWithFilterAst {
            request,
            filter_ast,
        };
        Ok(decode::<SearchResponse>(
            self.post_async("/v1/memories/search", &request, false)
                .await?,
        )?
        .data)
    }

    pub fn get(
        &self,
        memory_id: &str,
        user_id: &str,
        options: GetOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let client = self.clone();
        let memory_id = memory_id.to_owned();
        let user_id = user_id.to_owned();
        run_sync(async move { client.get_async(&memory_id, &user_id, options).await })
    }

    pub async fn get_async(
        &self,
        memory_id: &str,
        user_id: &str,
        options: GetOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let query = query(&[
            ("user_id", user_id.into()),
            ("include_inactive", options.include_inactive.to_string()),
        ]);
        let (status, body) = self
            .request_async(
                "GET",
                &format!("/v1/memories/{}?{query}", path_segment(memory_id)),
                None,
                false,
            )
            .await?;
        if status == 404 {
            return Ok(None);
        }
        ensure_success(status, &body)?;
        decode(body).map(Some)
    }

    pub fn list(&self, user_id: &str, options: ListOptions) -> Result<ListResponse, MemoryError> {
        let client = self.clone();
        let user_id = user_id.to_owned();
        run_sync(async move { client.list_async(&user_id, options).await })
    }

    pub async fn list_async(
        &self,
        user_id: &str,
        options: ListOptions,
    ) -> Result<ListResponse, MemoryError> {
        self.list_with_filter_ast_async(user_id, options, None)
            .await
    }

    pub fn list_with_filter_ast(
        &self,
        user_id: &str,
        options: ListOptions,
        filter_ast: Mem0FilterExpr,
    ) -> Result<ListResponse, MemoryError> {
        let client = self.clone();
        let user_id = user_id.to_owned();
        run_sync(async move {
            client
                .list_with_filter_ast_async(&user_id, options, Some(filter_ast))
                .await
        })
    }

    pub async fn list_with_filter_ast_async(
        &self,
        user_id: &str,
        options: ListOptions,
        filter_ast: Option<Mem0FilterExpr>,
    ) -> Result<ListResponse, MemoryError> {
        let mut pairs = vec![
            ("user_id", user_id.to_owned()),
            ("limit", options.limit.to_string()),
            ("include_inactive", options.include_inactive.to_string()),
        ];
        if let Some(value) = options.session_id {
            pairs.push(("session_id", value));
        }
        if let Some(value) = options.cursor {
            pairs.push(("cursor", value));
        }
        if let Some(filter_ast) = filter_ast {
            pairs.push((
                "filter_ast",
                serde_json::to_string(&filter_ast).map_err(config_error)?,
            ));
        }
        let (status, body) = self
            .request_async(
                "GET",
                &format!("/v1/memories?{}", query(&pairs)),
                None,
                false,
            )
            .await?;
        ensure_success(status, &body)?;
        decode(body)
    }

    pub fn update(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        options: UpdateOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        self.update_with_metadata(memory_id, user_id, content, None, options)
    }

    pub fn update_with_metadata(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
        options: UpdateOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let client = self.clone();
        let memory_id = memory_id.to_owned();
        let user_id = user_id.to_owned();
        let content = content.to_owned();
        run_sync(async move {
            client
                .update_async_with_metadata(&memory_id, &user_id, &content, metadata, options)
                .await
        })
    }

    /// Update memory content and metadata without changing its existing expiry.
    pub fn update_with_metadata_preserving_expiration(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
        options: UpdateOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let client = self.clone();
        let memory_id = memory_id.to_owned();
        let user_id = user_id.to_owned();
        let content = content.to_owned();
        run_sync(async move {
            client
                .update_async_with_metadata_mode(
                    &memory_id, &user_id, &content, metadata, options, true,
                )
                .await
        })
    }

    pub async fn update_async(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        options: UpdateOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        self.update_async_with_metadata(memory_id, user_id, content, None, options)
            .await
    }

    pub async fn update_async_with_metadata(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
        options: UpdateOptions,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        self.update_async_with_metadata_mode(memory_id, user_id, content, metadata, options, false)
            .await
    }

    async fn update_async_with_metadata_mode(
        &self,
        memory_id: &str,
        user_id: &str,
        content: &str,
        metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
        options: UpdateOptions,
        preserve_expiration: bool,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let payload = UpdateRequest {
            user_id: user_id.to_owned(),
            memory_id: memory_id.to_owned(),
            content: content.to_owned(),
            role: options.role,
            timestamp: options.timestamp,
            expires_at_ms: options.expires_at_ms,
        };
        let wrapped = UpdateRequestWithMetadata {
            request: payload,
            metadata,
            preserve_expiration,
        };
        let (status, body) = self
            .request_async(
                "PATCH",
                &format!("/v1/memories/{}", path_segment(memory_id)),
                Some(&json_value(&wrapped)?),
                true,
            )
            .await?;
        if status == 404 {
            return Ok(None);
        }
        ensure_success(status, &body)?;
        decode(body).map(Some)
    }

    pub fn delete(&self, memory_id: &str, user_id: &str) -> Result<bool, MemoryError> {
        let client = self.clone();
        let memory_id = memory_id.to_owned();
        let user_id = user_id.to_owned();
        run_sync(async move { client.delete_async(&memory_id, &user_id).await })
    }

    pub async fn delete_async(&self, memory_id: &str, user_id: &str) -> Result<bool, MemoryError> {
        let path = format!(
            "/v1/memories/{}?{}",
            path_segment(memory_id),
            query(&[("user_id", user_id.to_owned())])
        );
        let (status, body) = self.request_async("DELETE", &path, None, true).await?;
        if status == 404 {
            return Ok(false);
        }
        ensure_success(status, &body)?;
        Ok(true)
    }

    pub fn refresh(&self) -> Result<(), MemoryError> {
        let client = self.clone();
        run_sync(async move { client.refresh_async().await })
    }

    pub async fn refresh_async(&self) -> Result<(), MemoryError> {
        let (status, body) = self
            .request_async("POST", "/v1/memories/refresh", None, true)
            .await?;
        ensure_success(status, &body)
    }

    async fn post_async<T: Serialize>(
        &self,
        path: &str,
        payload: &T,
        may_mutate: bool,
    ) -> Result<Value, MemoryError> {
        let body = json_value(payload)?;
        let (status, response) = match (&self.connection, may_mutate, path) {
            (
                Connection::Project {
                    root,
                    resident_executable,
                },
                true,
                "/v1/memories" | "/v1/memories?wait_for_visibility=true",
            ) => {
                let (status, response, _) = crate::local_ipc::post_json_with_transport_async(
                    root,
                    path,
                    &body,
                    resident_executable.as_deref(),
                )
                .await
                .map_err(transport_error)?;
                (status.as_u16(), response)
            }
            _ => {
                self.request_async("POST", path, Some(&body), may_mutate)
                    .await?
            }
        };
        ensure_success(status, &response)?;
        Ok(response)
    }

    async fn request_async(
        &self,
        method: &str,
        path: &str,
        payload: Option<&Value>,
        may_mutate: bool,
    ) -> Result<(u16, Value), MemoryError> {
        self.request_with_transport_async(method, path, payload, may_mutate)
            .await
            .map(|(status, body, _)| (status, body))
    }

    async fn request_with_transport_async(
        &self,
        method: &str,
        path: &str,
        payload: Option<&Value>,
        may_mutate: bool,
    ) -> Result<(u16, Value, &'static str), MemoryError> {
        match &self.connection {
            Connection::Project {
                root,
                resident_executable,
            } => {
                let method = method.parse().map_err(config_error)?;
                let (status, body, transport) = crate::local_ipc::request_with_transport_async(
                    root,
                    method,
                    path,
                    payload,
                    may_mutate,
                    resident_executable.as_deref(),
                )
                .await
                .map_err(transport_error)?;
                Ok((status.as_u16(), body, transport))
            }
            Connection::Remote {
                base_url,
                async_client,
                api_key,
                ..
            } => {
                let method = method.parse().map_err(config_error)?;
                let mut request = async_client.request(method, format!("{base_url}{path}"));
                if let Some(token) = api_key {
                    request = request.bearer_auth(token);
                }
                if let Some(body) = payload {
                    request = request.json(body);
                }
                let response = request.send().await.map_err(|error| {
                    if error.is_connect() {
                        MemoryError::Unavailable {
                            message: error.to_string(),
                        }
                    } else if may_mutate {
                        MemoryError::Uncertain {
                            message: error.to_string(),
                        }
                    } else {
                        MemoryError::Unavailable {
                            message: error.to_string(),
                        }
                    }
                })?;
                let status = response.status().as_u16();
                let bytes = response.bytes().await.map_err(|error| {
                    if status == 408 {
                        MemoryError::Uncertain {
                            message: format!("timeout response body could not be read: {error}"),
                        }
                    } else if (400..500).contains(&status) {
                        MemoryError::Rejected {
                            status,
                            message: format!("error response body could not be read: {error}"),
                        }
                    } else if may_mutate {
                        MemoryError::Uncertain {
                            message: error.to_string(),
                        }
                    } else {
                        MemoryError::Unavailable {
                            message: error.to_string(),
                        }
                    }
                })?;
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).map_err(|error| {
                        if status == 408 {
                            MemoryError::Uncertain {
                                message: format!("invalid timeout response: {error}"),
                            }
                        } else if (400..500).contains(&status) {
                            MemoryError::Rejected {
                                status,
                                message: String::from_utf8_lossy(&bytes).into_owned(),
                            }
                        } else {
                            MemoryError::Uncertain {
                                message: format!("invalid JSON response: {error}"),
                            }
                        }
                    })?
                };
                Ok((status, body, "http"))
            }
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        payload: Option<&Value>,
        may_mutate: bool,
    ) -> Result<(u16, Value), MemoryError> {
        let client = self.clone();
        let method = method.to_owned();
        let path = path.to_owned();
        let payload = payload.cloned();
        run_sync(async move {
            client
                .request_async(&method, &path, payload.as_ref(), may_mutate)
                .await
        })
    }

    fn request_with_transport(
        &self,
        method: &str,
        path: &str,
        payload: Option<&Value>,
        may_mutate: bool,
    ) -> Result<(u16, Value, &'static str), MemoryError> {
        let client = self.clone();
        let method = method.to_owned();
        let path = path.to_owned();
        let payload = payload.cloned();
        run_sync(async move {
            client
                .request_with_transport_async(&method, &path, payload.as_ref(), may_mutate)
                .await
        })
    }
}

fn ensure_success(status: u16, body: &Value) -> Result<(), MemoryError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let message = body
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| body.to_string());
    if status == 408 {
        // A timeout response does not prove the server did not apply a write.
        Err(MemoryError::Uncertain { message })
    } else if (400..500).contains(&status) {
        // In particular, 429 is a definitive admission rejection. It is safe
        // to retry after backoff, retaining the same request ID for writes.
        Err(MemoryError::Rejected { status, message })
    } else {
        Err(MemoryError::Uncertain { message })
    }
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, MemoryError> {
    serde_json::from_value(value).map_err(|error| MemoryError::Uncertain {
        message: format!("invalid response payload: {error}"),
    })
}
fn json_value<T: Serialize>(value: &T) -> Result<Value, MemoryError> {
    serde_json::to_value(value).map_err(config_error)
}
fn transport_error(error: crate::local_ipc::ClientTransportError) -> MemoryError {
    match error {
        crate::local_ipc::ClientTransportError::BeforeDispatch(message) => {
            MemoryError::Unavailable { message }
        }
        crate::local_ipc::ClientTransportError::Uncertain(message) => {
            MemoryError::Uncertain { message }
        }
        crate::local_ipc::ClientTransportError::ResidentUnavailable(message) => {
            MemoryError::Unavailable { message }
        }
    }
}
fn config_error(error: impl std::fmt::Display) -> MemoryError {
    config(error.to_string())
}
fn config(message: impl Into<String>) -> MemoryError {
    MemoryError::Configuration {
        message: message.into(),
    }
}
pub(crate) fn generated_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "mem-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}
pub(crate) fn retry_session_id(user_id: &str, request_id: &str) -> String {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"membrick-retry-session-v2\0");
    digest.update((user_id.len() as u64).to_be_bytes());
    digest.update(user_id.as_bytes());
    digest.update((request_id.len() as u64).to_be_bytes());
    digest.update(request_id.as_bytes());
    format!("rest-retry-{:x}", digest.finalize())
}
fn path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", path_segment(key), path_segment(value)))
        .collect::<Vec<_>>()
        .join("&")
}

type SyncRuntimeJob = Box<dyn FnOnce(&tokio::runtime::Handle) + Send + 'static>;

fn sync_runtime_sender() -> Result<&'static Sender<SyncRuntimeJob>, MemoryError> {
    static SENDER: std::sync::OnceLock<Result<Sender<SyncRuntimeJob>, String>> =
        std::sync::OnceLock::new();
    match SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<SyncRuntimeJob>();
        std::thread::Builder::new()
            .name("membrick-sync-runtime".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!("could not start the SDK async runtime: {error}");
                        return;
                    }
                };
                let handle = runtime.handle().clone();
                while let Ok(job) = receiver.recv() {
                    job(&handle);
                }
            })
            .map(|_| sender)
            .map_err(|error| error.to_string())
    }) {
        Ok(sender) => Ok(sender),
        Err(message) => Err(MemoryError::Unavailable {
            message: format!("could not start SDK runtime worker: {message}"),
        }),
    }
}

fn run_sync<T, F>(future: F) -> Result<T, MemoryError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, MemoryError>> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    sync_runtime_sender()?
        .send(Box::new(move |runtime| {
            runtime.spawn(async move {
                let _ = sender.send(future.await);
            });
        }))
        .map_err(|error| MemoryError::Unavailable {
            message: format!("SDK runtime worker stopped: {error}"),
        })?;
    receiver.recv().map_err(|error| MemoryError::Unavailable {
        message: format!("SDK runtime worker stopped before replying: {error}"),
    })?
}

#[cfg(test)]
mod tests;
