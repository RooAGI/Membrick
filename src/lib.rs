//! Persistent agent memory through [`Memory`] and [`MemoryService`].
//!
//! Application clients should use [`Memory`] to connect to a project resident
//! or a remote service. The resident owns the service instance. Applications
//! deliberately embedding Membrick can construct [`MemoryService`] directly.
//!
//! ```no_run
//! use membrick::{MemoryService, PipelineOptions, AddRequest, Message};
//!
//! let mut memory = MemoryService::in_memory(PipelineOptions::default());
//! memory.add(AddRequest {
//!     request_id: "request-1".into(),
//!     user_id: "alice".into(),
//!     session_id: "session-1".into(),
//!     messages: vec![Message {
//!         role: "user".into(),
//!         content: "The release is on Friday.".into(),
//!         timestamp: None,
//!         expires_at_ms: None,
//!         supersedes_id: None,
//!     }],
//! })?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! Direct storage access is intentionally unavailable:
//!
//! ```compile_fail
//! use membrick::IndexStore;
//! ```
//! ```compile_fail
//! use membrick::index::MemoryIndex;
//! ```
//! ```compile_fail
//! use membrick::pipeline::build_index_store;
//! ```
//! ```compile_fail
//! use membrick::MemoryIndexSnapshot;
//! ```
//! ```compile_fail
//! use membrick::segments::SegmentedMemoryIndex;
//! ```
//! ```compile_fail
//! use membrick::build_query_snapshot;
//! ```

mod cli;
mod config;
mod conversation_state;
mod conversational_rerank;
mod daemon;
pub mod extension_activation;
pub mod extension_admission;
pub(crate) mod extension_host;
pub mod extension_policy_store;
pub mod extension_trust;
mod ids;
mod index;
#[cfg(any(
    feature = "claude-code",
    feature = "codex",
    feature = "gemini-cli",
    feature = "agy",
    feature = "muse-code",
    feature = "openclaw",
    feature = "roo-runtime"
))]
mod integrations;
mod lang;
mod logging;
/// Agent-facing search-result shaping, shared by the MCP `search` tools and
/// the retrieval benchmark so both present hits identically.
pub mod memory_api;
mod memory_client;
mod resident_executable;
/// Internal resident routes shared by Memory and the server router.
pub const MCP_TOOLS_CALL_PATH: &str = "/provider-memory/mcp/tools/call";
pub const MCP_DOCUMENTS_UPSERT_PATH: &str = "/provider-memory/mcp/documents/upsert";
/// Stable owner used for MCP-authored workspace memories.
pub const MCP_MEMORY_USER_ID: &str = "mcp";

mod pipeline;
mod provider_sources;
mod query_plan;
#[cfg(feature = "experimental")]
mod remote_query;
mod segments;
pub mod semantic_audit;
mod semantic_relations;
mod session_prepare;
mod source;
pub mod state_paths;
mod telemetry;
mod temporal_fact;

// Internal implementation details. These modules are intentionally not part
// of the public API; use the re-exports above instead.
mod adapters;
mod aggregation;
mod behood_query;
pub use behood_query::{analyze_evidence_signals_bulk, GroundedEvidenceSignal};
mod chunking;
mod claim_extractor;
#[cfg(feature = "experimental")]
mod corpus_graph;
mod engine;
mod filters;
mod graph;
#[cfg(feature = "experimental")]
mod ownership;
mod query_expansion;
pub(crate) use query_expansion::preload_lexical_store;
#[cfg(feature = "experimental")]
mod bart_tokenizer;
mod porter_stemmer;
mod query_semantics;
mod question_focus;
mod reasoning_rules;
mod report;
#[cfg(feature = "experimental")]
#[path = "code_review.rs"]
mod review;
mod rules;
mod semantic_tags;
mod stopwords_data;
#[cfg(feature = "experimental")]
mod symbols;
mod temporal;
mod tier1;
mod tier1_ner_daemon;
mod tokenizer;
mod updater;
#[cfg(feature = "experimental")]
mod usage;

// Public data contracts used by MemoryService. None provides direct storage access.
pub use crate::index::{QueryDiagnostics, QueryTimings, ScoreBreakdown, SearchResult};
pub use crate::lang::Lang;
#[deprecated(note = "use membrick::embedded::MemoryService")]
pub use crate::memory_api::MemoryService;
#[deprecated(note = "use the corresponding types under membrick::embedded")]
pub use crate::memory_api::{
    AddRequest, DeleteRequest, GetRequest, ListRequest, SearchRequest, SupersedeRequest,
    UpdateRequest,
};
pub use crate::memory_api::{
    AddRequestWithMetadata, AddResponse, ListResponse, MemoryRecord, Message, SearchMemory,
    SearchResponse, StagedAddResponse,
};
#[deprecated(note = "use MemoryError")]
#[allow(deprecated)]
pub use crate::memory_client::MemoryClientError;
#[deprecated(note = "use MemoryOptions")]
#[allow(deprecated)]
pub use crate::memory_client::MemoryClientOptions;
pub use crate::memory_client::{
    AddOptions, GetOptions, ListOptions, Memory, MemoryError, MemoryOptions, MemorySession,
    SearchOptions, UpdateOptions,
};

/// Agent and integration operations carried over the shared resident client.
pub mod agent {
    pub use crate::memory_client::{
        AgentOperationContext, CaptureReceipt, CaptureRequest, CaptureStatus,
        WorkspaceSearchResponse,
    };
}

/// Direct in-process service API for applications that intentionally embed Membrick.
/// Project applications should use [`Memory`] so the resident remains the sole writer.
pub mod embedded {
    pub use crate::memory_api::{
        AddRequest, AddRequestWithMetadata, DeleteRequest, GetRequest, ListRequest, MemoryService,
        SearchRequest, SupersedeRequest, UpdateRequest,
    };
    pub use crate::pipeline::PipelineOptions;
}
pub use crate::pipeline::{
    ChunkStrategy, IndexLocation, IndexStoreInspection, MemoryIndexLayout,
    MemoryIndexSegmentInspection, MemoryIndexSnapshotInspection, PipelineOptions, Tier1NerProvider,
    Tier1TermRankerKind,
};
pub use crate::segments::{SegmentRoutingStrategy, ShardQueryCompleteness, ShardQueryFailure};
pub use crate::semantic_relations::{SemanticStatus, SupersessionOptions};
pub use crate::source::{
    source_document_hash, KeyPhrase, KeyPhraseOrigin, SourceDocument, SourceDocumentEnrichment,
    SourceDocumentEvidence,
};
pub use crate::temporal::{FreshnessMode, FreshnessPolicy};
pub use crate::temporal_fact::TemporalFact;

// Internal aliases used by implementation modules and development tools.
pub(crate) use crate::aggregation::AggregateOutput;
pub(crate) use crate::index::TemporalQueryContext;
pub(crate) use crate::pipeline::{
    build_index_store, build_query_snapshot, build_query_snapshot_from_source_documents,
    default_production_pipeline_options, IndexStore, DEFAULT_SEGMENT_QUERY_TOP_N,
};
pub(crate) use crate::query_semantics::{analyze_query, parse_reference_date};
pub(crate) use crate::temporal::parse_temporal_date;
pub(crate) use crate::tier1::DEFAULT_SPACY_MODEL;
pub(crate) use crate::tier1_ner_daemon::NerDaemon;

mod executables;
mod install_telemetry;
mod local_ipc;

/// Launch a bundled executable. This process entry point exposes no memory internals.
#[doc(hidden)]
pub fn run_binary(name: &str) -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if install_telemetry::handle_invocation(name, &args) {
        return Ok(());
    }
    let role = if name == "server" {
        logging::Role::Server
    } else {
        logging::Role::from_args(&args)
    };
    // Residents, adapters and hooks start in their project root.
    logging::init(
        role,
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    if matches!(name, "membrick" | "lint-ai") {
        updater::check_for_update();
    }
    let result = executables::run(name);
    match (&result, role) {
        // A background process has no user to show the error to; record it
        // in the log format instead of letting the runtime print it bare.
        (Err(error), logging::Role::Resident | logging::Role::Mcp | logging::Role::Hook) => {
            tracing::error!("exiting: {error:#}");
            std::process::exit(1);
        }
        _ => result,
    }
}

#[cfg(feature = "python")]
use pyo3::prelude::*;
#[cfg(feature = "python")]
use pyo3::sync::PyOnceLock;
#[cfg(feature = "python")]
use pyo3::types::PyAny;
#[cfg(feature = "python")]
use pyo3::types::{PyDict, PyTuple, PyType};
#[cfg(feature = "python")]
#[cfg(feature = "python")]
use std::collections::BTreeMap;
#[cfg(feature = "python")]
use std::sync::Arc;

#[cfg(feature = "python")]
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(feature = "python")]
fn python_value_to_json(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let json_module = py.import("json")?;
    let encoded: String = json_module.getattr("dumps")?.call1((value,))?.extract()?;
    serde_json::from_str(&encoded).map_err(json_error)
}

#[cfg(feature = "python")]
fn json_to_python<T: serde::Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    let encoded = serde_json::to_string(value).map_err(json_error)?;
    let json_module = py.import("json")?;
    Ok(json_module.getattr("loads")?.call1((encoded,))?.unbind())
}

#[cfg(feature = "python")]
static PYTHON_MEMORY_ERRORS: PyOnceLock<[Py<PyType>; 4]> = PyOnceLock::new();

#[cfg(feature = "python")]
fn create_python_exception(
    py: Python<'_>,
    type_fn: &Bound<'_, PyAny>,
    name: &str,
    bases: Vec<Bound<'_, PyAny>>,
) -> PyResult<Py<PyType>> {
    let bases = PyTuple::new(py, bases)?;
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", "membrick")?;
    Ok(type_fn
        .call1((name, bases, namespace))?
        .cast_into::<PyType>()?
        .unbind())
}

#[cfg(feature = "python")]
fn initialize_python_memory_errors(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    if PYTHON_MEMORY_ERRORS.get(py).is_some() {
        return Ok(());
    }
    let builtins = py.import("builtins")?;
    let type_fn = builtins.getattr("type")?;
    let base = create_python_exception(
        py,
        &type_fn,
        "MembrickError",
        vec![builtins.getattr("Exception")?],
    )?;
    let base_any = base.bind(py).clone().into_any();
    let rejected = create_python_exception(
        py,
        &type_fn,
        "RejectedError",
        vec![builtins.getattr("ValueError")?, base_any.clone()],
    )?;
    let configuration = create_python_exception(
        py,
        &type_fn,
        "ConfigurationError",
        vec![builtins.getattr("ValueError")?, base_any.clone()],
    )?;
    let unavailable = create_python_exception(
        py,
        &type_fn,
        "UnavailableError",
        vec![builtins.getattr("ConnectionError")?, base_any.clone()],
    )?;
    let uncertain = create_python_exception(
        py,
        &type_fn,
        "UncertainError",
        vec![builtins.getattr("RuntimeError")?, base_any],
    )?;
    module.add("MembrickError", base.bind(py))?;
    module.add("RejectedError", rejected.bind(py))?;
    module.add("ConfigurationError", configuration.bind(py))?;
    module.add("UnavailableError", unavailable.bind(py))?;
    module.add("UncertainError", uncertain.bind(py))?;
    let _ = PYTHON_MEMORY_ERRORS.set(py, [rejected, configuration, unavailable, uncertain]);
    Ok(())
}

#[cfg(feature = "python")]
fn python_memory_error(py: Python<'_>, index: usize, message: impl Into<String>) -> pyo3::PyErr {
    let ty = PYTHON_MEMORY_ERRORS
        .get(py)
        .expect("Python Memory errors are initialized with the extension module")[index]
        .bind(py)
        .clone();
    pyo3::PyErr::from_type(ty, (message.into(),))
}

#[cfg(feature = "python")]
fn memory_client_error(py: Python<'_>, error: crate::memory_client::MemoryError) -> pyo3::PyErr {
    use crate::memory_client::MemoryError;
    let (index, message, status) = match error {
        MemoryError::Rejected { status, message } => (
            0,
            format!("request rejected ({status}): {message}"),
            Some(status),
        ),
        MemoryError::Configuration { mut message } => {
            message = message.replace(
                "initialize it explicitly with Memory::init",
                "initialize it explicitly with Memory.init",
            );
            (1, message, None)
        }
        MemoryError::Unavailable { message } => (2, message, None),
        MemoryError::Uncertain { message } => (3, format!("outcome uncertain: {message}"), None),
    };
    let error = python_memory_error(py, index, message);
    if let Some(status) = status {
        let _ = error.value(py).setattr("status", status);
    }
    error
}

#[cfg(feature = "python")]
fn json_error(error: impl std::fmt::Display) -> pyo3::PyErr {
    pyo3::exceptions::PyValueError::new_err(error.to_string())
}

#[cfg(feature = "python")]
#[pyclass(name = "Memory", unsendable)]
struct PySdkMemoryClient {
    client: Arc<crate::memory_client::Memory>,
}

#[cfg(feature = "python")]
#[pymethods]
impl PySdkMemoryClient {
    #[new]
    #[pyo3(signature = (path=None, base_url=None, api_key=None, request_timeout_ms=30000, resident_executable=None))]
    fn new(
        py: Python<'_>,
        path: Option<String>,
        base_url: Option<String>,
        api_key: Option<String>,
        request_timeout_ms: u64,
        resident_executable: Option<String>,
    ) -> PyResult<Self> {
        if path.is_some() && base_url.is_some() {
            return Err(python_memory_error(
                py,
                1,
                "path and base_url cannot both be set",
            ));
        }
        let launch_options = crate::memory_client::MemoryOptions {
            request_timeout: std::time::Duration::from_millis(request_timeout_ms),
            api_key: api_key.clone(),
            resident_executable: resident_executable
                .map(std::path::PathBuf::from)
                .or_else(|| python_resident_executable(py)),
        };
        let client = match base_url {
            Some(base_url) => crate::memory_client::Memory::remote(base_url, launch_options),
            None => {
                let Some(path) = path else {
                    return Err(python_memory_error(
                        py,
                        1,
                        "provide path for a project connection or base_url for a remote connection",
                    ));
                };
                crate::memory_client::Memory::with_options(path, launch_options)
            }
        }
        .map_err(|error| memory_client_error(py, error))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    #[staticmethod]
    #[pyo3(signature = (path, api_key=None, request_timeout_ms=30000, resident_executable=None))]
    fn init(
        py: Python<'_>,
        path: String,
        api_key: Option<String>,
        request_timeout_ms: u64,
        resident_executable: Option<String>,
    ) -> PyResult<Self> {
        let options = crate::memory_client::MemoryOptions {
            api_key,
            request_timeout: std::time::Duration::from_millis(request_timeout_ms),
            resident_executable: resident_executable
                .map(std::path::PathBuf::from)
                .or_else(|| python_resident_executable(py)),
        };
        let client = crate::memory_client::Memory::init_with_options(path, options)
            .map_err(|error| memory_client_error(py, error))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    #[pyo3(signature = (messages, user_id, request_id=None, session_id=None, wait_for_visibility=false))]
    fn add(
        &self,
        py: Python<'_>,
        messages: &Bound<'_, PyAny>,
        user_id: String,
        request_id: Option<String>,
        session_id: Option<String>,
        wait_for_visibility: bool,
    ) -> PyResult<Py<PyAny>> {
        let messages = python_value_to_json(py, messages)?;
        let messages =
            serde_json::from_value::<Vec<memory_api::Message>>(messages).map_err(json_error)?;
        let client = self.client.clone();
        let response = py
            .detach(move || {
                client.add(
                    messages,
                    &user_id,
                    crate::memory_client::AddOptions {
                        request_id,
                        session_id,
                        wait_for_visibility,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &response)
    }

    fn add_transaction(&self, py: Python<'_>, requests: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let requests = python_value_to_json(py, requests)?;
        let requests = serde_json::from_value::<Vec<memory_api::AddRequestWithMetadata>>(requests)
            .map_err(json_error)?;
        let client = self.client.clone();
        let responses = py
            .detach(move || client.add_transaction(requests))
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &responses)
    }

    #[pyo3(signature = (query, user_id, top_k=10, session_id=None, filters=None, scope=None, reference_date=None, options=None))]
    fn search(
        &self,
        py: Python<'_>,
        query: String,
        user_id: String,
        top_k: usize,
        session_id: Option<String>,
        filters: Option<BTreeMap<String, String>>,
        scope: Option<String>,
        reference_date: Option<String>,
        options: Option<Vec<String>>,
    ) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let result = py
            .detach(move || {
                client.search(
                    &query,
                    &user_id,
                    crate::memory_client::SearchOptions {
                        top_k,
                        session_id,
                        filters,
                        scope,
                        reference_date,
                        options,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (memory_id, user_id, include_inactive=false))]
    fn get(
        &self,
        py: Python<'_>,
        memory_id: String,
        user_id: String,
        include_inactive: bool,
    ) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let result = py
            .detach(move || {
                client.get(
                    &memory_id,
                    &user_id,
                    crate::memory_client::GetOptions { include_inactive },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (user_id, session_id=None, limit=100, cursor=None, include_inactive=false))]
    fn list(
        &self,
        py: Python<'_>,
        user_id: String,
        session_id: Option<String>,
        limit: usize,
        cursor: Option<String>,
        include_inactive: bool,
    ) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let result = py
            .detach(move || {
                client.list(
                    &user_id,
                    crate::memory_client::ListOptions {
                        session_id,
                        limit,
                        cursor,
                        include_inactive,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (memory_id, user_id, content, role=None, timestamp=None, expires_at_ms=None))]
    fn update(
        &self,
        py: Python<'_>,
        memory_id: String,
        user_id: String,
        content: String,
        role: Option<String>,
        timestamp: Option<i64>,
        expires_at_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let result = py
            .detach(move || {
                client.update(
                    &memory_id,
                    &user_id,
                    &content,
                    crate::memory_client::UpdateOptions {
                        role,
                        timestamp,
                        expires_at_ms,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    fn delete(&self, py: Python<'_>, memory_id: String, user_id: String) -> PyResult<bool> {
        let client = self.client.clone();
        py.detach(move || client.delete(&memory_id, &user_id))
            .map_err(|error| memory_client_error(py, error))
    }

    fn refresh(&self, py: Python<'_>) -> PyResult<()> {
        let client = self.client.clone();
        py.detach(move || client.refresh())
            .map_err(|error| memory_client_error(py, error))
    }

    fn close(&self) {
        self.client.close();
    }

    fn session(&self, user_id: String, session_id: String) -> PySdkMemorySession {
        PySdkMemorySession {
            client: Arc::new((*self.client).clone()),
            user_id,
            session_id,
        }
    }
}

#[cfg(feature = "python")]
fn python_resident_executable(py: Python<'_>) -> Option<std::path::PathBuf> {
    let scripts: String = py
        .import("sysconfig")
        .ok()?
        .call_method1("get_path", ("scripts",))
        .ok()?
        .extract()
        .ok()?;
    let executable = std::path::Path::new(&scripts).join(if cfg!(windows) {
        "membrick.exe"
    } else {
        "membrick"
    });
    executable.is_file().then_some(executable)
}

/// Opt-in Mem0-style call and result adapter over an existing native client.
/// This intentionally implements only the subset documented in
/// `docs/mem0-compat.md`; it is not a drop-in Mem0 replacement.
#[cfg(feature = "python")]
#[pyclass(name = "Mem0Adapter", unsendable)]
struct PyMem0Adapter {
    client: Arc<crate::memory_client::Memory>,
}

#[cfg(feature = "python")]
#[pymethods]
impl PyMem0Adapter {
    #[new]
    fn new(client: PyRef<'_, PySdkMemoryClient>) -> Self {
        Self {
            client: Arc::new((*client.client).clone()),
        }
    }

    fn close(&self) {
        self.client.close();
    }

    #[pyo3(signature = (messages, user_id, run_id=None, agent_id=None, app_id=None, metadata=None, infer=false, request_id=None, expiration_date=None))]
    fn add(
        &self,
        py: Python<'_>,
        messages: &Bound<'_, PyAny>,
        user_id: String,
        run_id: Option<String>,
        agent_id: Option<String>,
        app_id: Option<String>,
        metadata: Option<&Bound<'_, PyAny>>,
        infer: Option<bool>,
        request_id: Option<String>,
        expiration_date: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        reject_mem0_options(&[
            ("agent_id", agent_id.is_some()),
            ("app_id", app_id.is_some()),
        ])?;
        if infer != Some(false) {
            return Err(pyo3::exceptions::PyNotImplementedError::new_err(
                "Membrick requires infer=False for message storage; infer=True uses Mem0 inference semantics that Membrick does not implement",
            ));
        }
        let expires_at_ms = match expiration_date.as_deref() {
            Some(date) => {
                let date =
                    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|error| {
                        pyo3::exceptions::PyValueError::new_err(format!(
                            "expiration_date must be YYYY-MM-DD: {error}"
                        ))
                    })?;
                let exclusive_end = date.succ_opt().ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err(
                        "expiration_date must have a representable following UTC date",
                    )
                })?;
                let timestamp_ms = exclusive_end
                    .and_hms_opt(0, 0, 0)
                    .expect("valid midnight")
                    .and_utc()
                    .timestamp_millis();
                if timestamp_ms <= 0 {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "expiration_date must not precede the Unix epoch",
                    ));
                }
                Some(u64::try_from(timestamp_ms).map_err(|_| {
                    pyo3::exceptions::PyValueError::new_err(
                        "expiration_date must not precede the Unix epoch",
                    )
                })?)
            }
            None => None,
        };
        let encoded = python_value_to_json(py, messages)?;
        let mut messages = match encoded {
            serde_json::Value::String(content) => vec![memory_api::Message {
                role: "user".to_string(),
                content,
                timestamp: None,
                expires_at_ms: None,
                supersedes_id: None,
            }],
            value @ serde_json::Value::Object(_) => {
                vec![serde_json::from_value::<memory_api::Message>(value).map_err(json_error)?]
            }
            value @ serde_json::Value::Array(_) => {
                serde_json::from_value::<Vec<memory_api::Message>>(value).map_err(json_error)?
            }
            _ => {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "messages must be str, dict, or list[dict]",
                ))
            }
        };
        let metadata = metadata
            .map(|value| python_value_to_json(py, value))
            .transpose()?
            .map(|value| serde_json::from_value::<BTreeMap<String, serde_json::Value>>(value))
            .transpose()
            .map_err(json_error)?
            .unwrap_or_default();
        if let Some(expires_at_ms) = expires_at_ms {
            for message in &mut messages {
                if message
                    .expires_at_ms
                    .is_some_and(|value| value != expires_at_ms)
                {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "expiration_date conflicts with a message expires_at_ms",
                    ));
                }
                message.expires_at_ms = Some(expires_at_ms);
            }
        }
        let client = self.client.clone();
        let include_run_id = run_id.is_some();
        let requested_session_id = run_id.clone();
        let request_id = request_id.unwrap_or_else(crate::memory_client::generated_id);
        let result = py
            .detach(move || {
                let receipt = client.add_with_metadata(
                    messages,
                    &user_id,
                    metadata,
                    crate::memory_client::AddOptions {
                        request_id: Some(request_id),
                        session_id: requested_session_id,
                        wait_for_visibility: true,
                        ..Default::default()
                    },
                )?;
                let doc_ids = receipt
                    .adjudication
                    .as_ref()
                    .map(|adjudication| adjudication.doc_ids.clone())
                    .ok_or_else(|| crate::memory_client::MemoryError::Uncertain {
                        message: format!(
                            "add succeeded but the resident omitted its write receipt (request_id={})",
                            receipt.request_id
                        ),
                    })?;
                let resolved_session_id = receipt.session_id.clone();
                let mut results = Vec::with_capacity(doc_ids.len());
                for memory_id in doc_ids {
                    let record = client
                        .get(
                            &memory_id,
                            &receipt.user_id,
                            crate::memory_client::GetOptions::default(),
                        )?
                        .ok_or_else(|| crate::memory_client::MemoryError::Uncertain {
                            message: format!(
                                "add succeeded but its result is not visible yet (request_id={}, memory_id={memory_id})",
                                receipt.request_id
                            ),
                        })?;
                    let mut item = mem0_memory_record(record);
                    if !include_run_id {
                        item.as_object_mut().map(|object| object.remove("run_id"));
                    } else {
                        item["run_id"] = serde_json::Value::String(resolved_session_id.clone());
                    }
                    results.push(item);
                }
                Ok::<_, crate::memory_client::MemoryError>(serde_json::json!({"results": results}))
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (query, user_id=None, top_k=None, filters=None, run_id=None, agent_id=None, app_id=None, threshold=None, rerank=None, show_expired=false, limit=None))]
    fn search(
        &self,
        py: Python<'_>,
        query: String,
        user_id: Option<String>,
        top_k: Option<usize>,
        filters: Option<&Bound<'_, PyAny>>,
        run_id: Option<String>,
        agent_id: Option<String>,
        app_id: Option<String>,
        threshold: Option<f32>,
        rerank: Option<bool>,
        show_expired: bool,
        limit: Option<usize>,
    ) -> PyResult<Py<PyAny>> {
        reject_mem0_options(&[
            ("agent_id", agent_id.is_some()),
            ("app_id", app_id.is_some()),
            ("threshold", threshold.is_some()),
            ("rerank", rerank.is_some()),
            ("show_expired", show_expired),
        ])?;
        let top_k = match (top_k, limit) {
            (Some(_), Some(_)) => {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "pass either top_k or the legacy limit alias, not both",
                ));
            }
            (Some(top_k), None) => top_k,
            (None, Some(limit)) => limit,
            (None, None) => 10,
        };
        if top_k == 0 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Mem0Adapter.search top_k must be greater than zero",
            ));
        }
        if top_k > 100 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Membrick search supports top_k up to 100",
            ));
        }
        let filter_ast = filters
            .map(|value| python_value_to_json(py, value))
            .transpose()?
            .map(memory_api::Mem0FilterExpr::from_platform_value)
            .transpose()
            .map_err(|error| pyo3::exceptions::PyValueError::new_err(error.to_string()))?;
        if let Some(ast) = filter_ast.as_ref() {
            ast.validate_mem0_fields().map_err(|error| {
                pyo3::exceptions::PyNotImplementedError::new_err(error.to_string())
            })?;
        }
        let mut filter_users = Vec::new();
        if let Some(ast) = filter_ast.as_ref() {
            ast.user_ids(&mut filter_users);
        }
        filter_users.sort();
        filter_users.dedup();
        let user_id = match (user_id, filter_users.as_slice()) {
            (Some(user_id), [filtered]) if user_id != *filtered => {
                return Err(pyo3::exceptions::PyValueError::new_err("user_id conflicts with filters"));
            }
            (Some(user_id), _) => user_id,
            (None, [filtered]) => filtered.clone(),
            (None, _) => return Err(pyo3::exceptions::PyValueError::new_err(
                "search requires user_id or one equality filter for user_id; Membrick requires explicit ownership context",
            )),
        };
        if filter_users.iter().any(|filtered| filtered != &user_id) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "filters contain a user_id outside the requested owner scope",
            ));
        }
        let client = self.client.clone();
        let filter_ast_for_search = filter_ast.clone();
        let result = py
            .detach(move || {
                mem0_search_with_client(
                    &client,
                    &query,
                    &user_id,
                    top_k,
                    run_id.as_deref(),
                    filter_ast_for_search,
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    /// Return a Mem0 Platform-shaped paginated envelope for the supported
    /// user and flat metadata filters. Pagination links use the `membrick:`
    /// URI scheme because this adapter is backed by a MemoryClient, not a
    /// Mem0 HTTP endpoint.
    #[pyo3(signature = (filters=None, page=1, page_size=50, show_expired=false))]
    fn get_all(
        &self,
        py: Python<'_>,
        filters: Option<&Bound<'_, PyAny>>,
        page: usize,
        page_size: usize,
        show_expired: bool,
    ) -> PyResult<Py<PyAny>> {
        if page == 0 || page_size == 0 || page_size > 200 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "get_all page must be >= 1 and page_size must be between 1 and 200",
            ));
        }
        if show_expired {
            return Err(pyo3::exceptions::PyNotImplementedError::new_err(
                "Membrick does not support get_all(show_expired=True)",
            ));
        }
        let filter_ast = filters
            .map(|value| python_value_to_json(py, value))
            .transpose()?
            .map(memory_api::Mem0FilterExpr::from_platform_value)
            .transpose()
            .map_err(|error| pyo3::exceptions::PyValueError::new_err(error.to_string()))?
            .ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(
                    "get_all requires filters with a user_id owner scope",
                )
            })?;
        filter_ast
            .validate_mem0_fields()
            .map_err(|error| pyo3::exceptions::PyNotImplementedError::new_err(error.to_string()))?;
        let mut filter_users = Vec::new();
        filter_ast.user_ids(&mut filter_users);
        filter_users.sort();
        filter_users.dedup();
        if filter_users.len() != 1 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "get_all requires exactly one equality filter for user_id; Membrick requires explicit ownership context",
            ));
        }
        let user_id = filter_users.remove(0);
        let mut required_run_ids = Vec::new();
        filter_ast.run_ids(&mut required_run_ids);
        required_run_ids.sort();
        required_run_ids.dedup();
        let attributed_run_id = match required_run_ids.as_slice() {
            [run_id] => Some(run_id.clone()),
            _ => None,
        };
        let client = self.client.clone();
        let result = py
            .detach(move || {
                let start = page.saturating_sub(1).saturating_mul(page_size);
                let end = start.saturating_add(page_size);
                let mut count = 0usize;
                let mut results = Vec::with_capacity(page_size);
                let mut cursor = None;
                let mut scanned = 0usize;
                loop {
                    let response = client.list_with_filter_ast(
                        &user_id,
                        crate::memory_client::ListOptions {
                            session_id: None,
                            limit: 100,
                            cursor,
                            include_inactive: false,
                            ..Default::default()
                        },
                        filter_ast.clone(),
                    )?;
                    if scanned == 0 {
                        count = response.total_count;
                        if start >= count {
                            break;
                        }
                    }
                    for record in response.data {
                        if scanned >= start && scanned < end {
                            let mut item = mem0_memory_record(record);
                            if let (Some(requested_run_id), Some(session_id)) = (
                                attributed_run_id.as_deref(),
                                item.get("session_id").and_then(|v| v.as_str()),
                            ) {
                                if session_id == requested_run_id {
                                    item["run_id"] =
                                        serde_json::Value::String(session_id.to_owned());
                                }
                            }
                            results.push(item);
                        }
                        scanned = scanned.saturating_add(1);
                    }
                    if scanned >= end {
                        break;
                    }
                    let Some(next) = response.next_cursor else {
                        break;
                    };
                    cursor = Some(next);
                }
                let page_link = |target_page: usize| {
                    let mut pairs = vec![
                        ("page".to_string(), target_page.to_string()),
                        ("page_size".to_string(), page_size.to_string()),
                        ("user_id".to_string(), user_id.clone()),
                    ];
                    pairs.push((
                        "filter_ast".to_string(),
                        serde_json::to_string(&filter_ast).expect("typed filter AST serializes"),
                    ));
                    let query = pairs
                        .iter()
                        .map(|(key, value)| {
                            format!(
                                "{}={}",
                                mem0_query_component(key),
                                mem0_query_component(value)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("&");
                    format!("membrick://mem0-adapter/get_all?{query}")
                };
                let previous = (page > 1).then(|| page_link(page - 1));
                let next = (start.saturating_add(page_size) < count).then(|| page_link(page + 1));
                Ok::<_, crate::memory_client::MemoryError>(serde_json::json!({
                    "count": count,
                    "next": next,
                    "previous": previous,
                    "results": results,
                }))
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (memory_id, user_id))]
    fn get(&self, py: Python<'_>, memory_id: String, user_id: String) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let record = py
            .detach(move || {
                client.get(
                    &memory_id,
                    &user_id,
                    crate::memory_client::GetOptions::default(),
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        match record {
            Some(record) => json_to_python(py, &mem0_memory_record(record)),
            None => Err(pyo3::exceptions::PyValueError::new_err(
                "404 Memory not found",
            )),
        }
    }

    #[pyo3(signature = (memory_id, text=None, user_id=None, metadata=None, data=None))]
    /// Update a memory while enforcing explicit ownership with `user_id`.
    /// Platform uses `text` and does not accept `user_id`; Membrick requires
    /// the owner explicitly. When supplied, metadata replaces user metadata.
    /// An omitted expiration is preserved for this adapter update.
    fn update(
        &self,
        py: Python<'_>,
        memory_id: String,
        text: Option<String>,
        user_id: Option<String>,
        metadata: Option<&Bound<'_, PyAny>>,
        data: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let text = text
            .or(data)
            .ok_or_else(|| pyo3::exceptions::PyTypeError::new_err("update requires text"))?;
        let user_id = user_id.ok_or_else(|| {
            pyo3::exceptions::PyTypeError::new_err("update requires user_id for ownership")
        })?;
        let metadata = metadata
            .map(|value| python_value_to_json(py, value))
            .transpose()?
            .map(|value| serde_json::from_value::<BTreeMap<String, serde_json::Value>>(value))
            .transpose()
            .map_err(json_error)?;
        let client = self.client.clone();
        let record = py
            .detach(move || {
                client.update_with_metadata_preserving_expiration(
                    &memory_id,
                    &user_id,
                    &text,
                    metadata,
                    crate::memory_client::UpdateOptions::default(),
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        match record {
            Some(record) => json_to_python(py, &mem0_memory_record(record)),
            None => Err(pyo3::exceptions::PyValueError::new_err(
                "404 Memory not found",
            )),
        }
    }

    #[pyo3(signature = (memory_id, user_id))]
    fn delete(&self, py: Python<'_>, memory_id: String, user_id: String) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let deleted = py
            .detach(move || client.delete(&memory_id, &user_id))
            .map_err(|error| memory_client_error(py, error))?;
        if !deleted {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "404 Memory not found",
            ));
        }
        json_to_python(
            py,
            &serde_json::json!({"message": "Memory deleted successfully!"}),
        )
    }
}

#[cfg(feature = "python")]
fn mem0_memory_record(record: memory_api::MemoryRecord) -> serde_json::Value {
    let content = record
        .role
        .as_deref()
        .and_then(|role| record.content.strip_prefix(&format!("{role}: ")))
        .unwrap_or(&record.content);
    let mut value = serde_json::json!({
        "id": record.id,
        "memory": content,
        "user_id": record.user_id,
        "metadata": memory_api::decode_mem0_metadata(record.metadata),
        "session_id": record.session_id.clone(),
    });
    if let Some(created_at) = record.created_at {
        value["created_at"] = serde_json::Value::String(created_at);
    }
    if let Some(expiration_date) = mem0_expiration_date(record.expires_at_ms) {
        value["expiration_date"] = serde_json::Value::String(expiration_date);
    }
    value
}

#[cfg(feature = "python")]
fn mem0_search_memory(hit: memory_api::SearchMemory) -> serde_json::Value {
    let content = hit
        .role
        .as_deref()
        .and_then(|role| hit.content.strip_prefix(&format!("{role}: ")))
        .unwrap_or(&hit.content);
    let mut value = serde_json::json!({
        "id": hit.id,
        "memory": content,
        "user_id": hit.user_id.unwrap_or_default(),
        "metadata": memory_api::decode_mem0_metadata(hit.metadata),
        "session_id": hit.session_id,
        "score": hit.score,
    });
    if let Some(created_at) = hit.created_at {
        value["created_at"] = serde_json::Value::String(created_at);
    }
    if let Some(expiration_date) = mem0_expiration_date(hit.expires_at_ms) {
        value["expiration_date"] = serde_json::Value::String(expiration_date);
    }
    if let Some(supersedes_id) = hit.supersedes_id {
        value["supersedes_id"] = serde_json::Value::String(supersedes_id);
    }
    value
}

#[cfg(feature = "python")]
fn mem0_expiration_date(expires_at_ms: Option<u64>) -> Option<String> {
    let expires_at_ms = i64::try_from(expires_at_ms?).ok()?;
    let expiration_boundary = chrono::DateTime::from_timestamp_millis(expires_at_ms)?;
    let boundary_date = expiration_boundary.date_naive();
    let expiration_date = if expiration_boundary.time()
        == chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid midnight")
    {
        boundary_date.pred_opt().unwrap_or(boundary_date)
    } else {
        boundary_date
    };
    Some(expiration_date.format("%Y-%m-%d").to_string())
}

#[cfg(feature = "python")]
fn mem0_search_with_client(
    client: &crate::memory_client::Memory,
    query: &str,
    user_id: &str,
    top_k: usize,
    run_id: Option<&str>,
    filter_ast: Option<memory_api::Mem0FilterExpr>,
) -> Result<serde_json::Value, crate::memory_client::MemoryError> {
    let options = crate::memory_client::SearchOptions {
        top_k,
        session_id: run_id.map(str::to_owned),
        ..Default::default()
    };
    let hits = match filter_ast {
        Some(filter_ast) => client.search_with_filter_ast(query, user_id, options, filter_ast)?,
        None => client.search(query, user_id, options)?,
    };
    let mut results = hits.into_iter().map(mem0_search_memory).collect::<Vec<_>>();
    for item in &mut results {
        if let (Some(requested_run_id), Some(session_id)) = (
            run_id,
            item.get("session_id").and_then(|value| value.as_str()),
        ) {
            if session_id == requested_run_id {
                item["run_id"] = serde_json::Value::String(session_id.to_owned());
            }
        }
    }
    Ok(serde_json::json!({"results": results}))
}

#[cfg(feature = "python")]
fn mem0_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(feature = "python")]
fn reject_mem0_options(options: &[(&str, bool)]) -> PyResult<()> {
    if let Some((name, _)) = options.iter().find(|(_, supplied)| *supplied) {
        return Err(pyo3::exceptions::PyNotImplementedError::new_err(format!(
            "Mem0Adapter does not support the '{name}' option; see docs/mem0-compat.md"
        )));
    }
    Ok(())
}

#[cfg(feature = "python")]
#[pyclass(name = "MemorySession", unsendable)]
struct PySdkMemorySession {
    client: Arc<crate::memory_client::Memory>,
    user_id: String,
    session_id: String,
}

#[cfg(feature = "python")]
#[pymethods]
impl PySdkMemorySession {
    #[getter]
    fn user_id(&self) -> &str {
        &self.user_id
    }
    #[getter]
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn close(&self) {
        self.client.close();
    }

    #[pyo3(signature = (messages, request_id=None, wait_for_visibility=false))]
    fn add(
        &self,
        py: Python<'_>,
        messages: &Bound<'_, PyAny>,
        request_id: Option<String>,
        wait_for_visibility: bool,
    ) -> PyResult<Py<PyAny>> {
        let messages = python_value_to_json(py, messages)?;
        let messages =
            serde_json::from_value::<Vec<memory_api::Message>>(messages).map_err(json_error)?;
        let client = self.client.clone();
        let user_id = self.user_id.clone();
        let session_id = self.session_id.clone();
        let result = py
            .detach(move || {
                client.add(
                    messages,
                    &user_id,
                    crate::memory_client::AddOptions {
                        request_id,
                        session_id: Some(session_id),
                        wait_for_visibility,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }

    #[pyo3(signature = (query, top_k=10, filters=None, scope=None, reference_date=None, options=None))]
    fn search(
        &self,
        py: Python<'_>,
        query: String,
        top_k: usize,
        filters: Option<BTreeMap<String, String>>,
        scope: Option<String>,
        reference_date: Option<String>,
        options: Option<Vec<String>>,
    ) -> PyResult<Py<PyAny>> {
        let client = self.client.clone();
        let user_id = self.user_id.clone();
        let session_id = self.session_id.clone();
        let result = py
            .detach(move || {
                client.search(
                    &query,
                    &user_id,
                    crate::memory_client::SearchOptions {
                        top_k,
                        session_id: Some(session_id),
                        filters,
                        scope,
                        reference_date,
                        options,
                    },
                )
            })
            .map_err(|error| memory_client_error(py, error))?;
        json_to_python(py, &result)
    }
}

#[cfg(feature = "python")]
#[pymodule]
fn membrick(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    initialize_python_memory_errors(py, m)?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_class::<PySdkMemoryClient>()?;
    m.add_class::<PyMem0Adapter>()?;
    m.add_class::<PySdkMemorySession>()?;
    Ok(())
}

#[cfg(test)]
mod internal_tests;
