#[path = "server_writer.rs"]
mod server_writer;
use server_writer::StagedWriter;

#[path = "server_dashboard_routes.rs"]
mod server_dashboard_routes;
#[path = "server_extension_events.rs"]
mod server_extension_events;
#[path = "server_memory_routes.rs"]
mod server_memory_routes;
#[path = "server_operations_routes.rs"]
mod server_operations_routes;
#[path = "server_provider_routes.rs"]
mod server_provider_routes;

use crate::memory_api::{
    AddRequest, AddRequestWithMetadata, DeleteRequest, GetRequest, ListRequest, Mem0FilterExpr,
    MemoryService, MemoryStore, SearchRequest, SearchRequestWithFilterAst, SupersedeRequest,
    UpdateRequestWithMetadata,
};
use crate::segments::SegmentRoutingStrategy;
use crate::telemetry::{
    project_query_snapshot, provider_lifecycle_status, OperationalTelemetry,
    ProviderLifecycleEvent, TelemetrySnapshot,
};
use crate::{
    default_production_pipeline_options, lang::Lang, IndexStoreInspection, MemoryIndexLayout,
    PipelineOptions, DEFAULT_SEGMENT_QUERY_TOP_N, MCP_DOCUMENTS_UPSERT_PATH, MCP_TOOLS_CALL_PATH,
};
use anyhow::Context;
use axum::{
    body::Body,
    error_handling::HandleErrorLayer,
    extract::{Extension, Json, Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect},
    routing::{any, get, post},
    Router,
};
use clap::{CommandFactory, FromArgMatches, Parser};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tower::{BoxError, ServiceBuilder};

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 128;
const DEFAULT_MAX_ACTIVE_WORKSPACES: usize = 8;

#[derive(Debug, Parser)]
#[command(name = "membrick-server", about = "Memory Add/Search server backed by Membrick", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// Read server defaults from a TOML settings file. CLI flags override it.
    #[arg(long)]
    settings: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,
    #[arg(long)]
    index: Option<PathBuf>,
    /// Publish staged writes after this interval from the first pending add.
    #[arg(long, default_value_t = 250)]
    refresh_interval_ms: u64,
    /// Publish early when this many documents are pending.
    #[arg(long, default_value_t = 512)]
    refresh_batch_size: usize,
    /// Persist a full checkpoint and reclaim the durable journal on this cadence.
    #[arg(long, default_value_t = 30)]
    checkpoint_interval_seconds: u64,
    #[arg(long)]
    server_token: Option<String>,
    #[arg(long)]
    tenant_id: Option<String>,
    #[arg(long)]
    allow_unauthenticated: bool,
    /// Allow binding to non-loopback interfaces. Requires token or JWT authentication.
    #[arg(long)]
    allow_non_loopback: bool,
    /// Enable adaptive segmented routing up to this many segments.
    #[arg(long)]
    adaptive_segment_max_n: Option<usize>,
    /// Use the global single-index layout for controlled comparisons.
    #[arg(long)]
    single_index: bool,
    /// Query every segmented shard (global segmented comparison mode).
    #[arg(long)]
    global_index: bool,
    /// Fuse the corpus-wide global arm with the routed arm
    /// (higher recall, higher latency; off by default).
    #[arg(long)]
    fuse_global: bool,
    /// Disable the two-stage conversational rerank for session follow-ups.
    #[arg(long)]
    no_conversational_rerank: bool,
    /// Enable optional Bekind linguistic enrichment (disabled by default).
    #[arg(long)]
    bekind: bool,
    /// Segment routing strategy for the segmented layout.
    /// Names match docs/benchmark-results.md. Coverage-local is the default;
    /// other strategies remain selectable for quality comparisons.
    #[arg(long, value_enum, default_value_t = SegmentRoutingArg::CoverageLocal)]
    segment_routing: SegmentRoutingArg,
    /// Content language. `auto` (default) detects per text from script
    /// statistics (plus Spanish signals for Latin text); pass `zh`/`ko`/`es`/`en` to force it.
    #[arg(long, value_enum, default_value = "auto")]
    lang: Lang,
    /// Project root containing provider hook telemetry under `.membrick`.
    #[arg(long)]
    project_root: Option<PathBuf>,
    /// Serve the resident workspace API over local IPC instead of TCP.
    #[arg(long, hide = true)]
    resident_server: bool,
    /// Stop this automatically started resident when provider leases end.
    #[arg(long, hide = true)]
    managed_resident: bool,
    /// Additional exact project roots this server may open. Repeat for multiple projects.
    #[arg(long = "workspace-root", value_name = "PATH")]
    workspace_roots: Vec<PathBuf>,
    /// Maximum number of resident workspace services, including the primary workspace.
    #[arg(long, default_value_t = DEFAULT_MAX_ACTIVE_WORKSPACES)]
    max_active_workspaces: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ServerSettings {
    bind: Option<String>,
    resident_http_bind: Option<String>,
    resident_http_auto_port: Option<bool>,
    resident_idle_timeout_seconds: Option<u64>,
    index: Option<PathBuf>,
    project_root: Option<PathBuf>,
    server_token: Option<String>,
    server_token_file: Option<PathBuf>,
    tenant_id: Option<String>,
    jwt_secret: Option<String>,
    adaptive_segment_max_n: Option<usize>,
    freshness: Option<crate::temporal::FreshnessPolicy>,
}

fn settings_path(args: &Args) -> anyhow::Result<PathBuf> {
    if let Some(path) = &args.settings {
        return Ok(path.clone());
    }
    let root = args
        .project_root
        .clone()
        .unwrap_or(std::env::current_dir()?);
    Ok(root.join(".membrick").join("server.toml"))
}

fn load_server_settings(path: &std::path::Path) -> anyhow::Result<ServerSettings> {
    if !path.exists() {
        return Ok(ServerSettings::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    toml::from_str(&raw)
        .with_context(|| format!("failed to parse settings file {}", path.display()))
}

/// CLI-selectable segment routing strategies. Variant names map to the router
/// names in docs/benchmark-results.md; see SegmentRoutingStrategy for the
/// scoring each one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SegmentRoutingArg {
    Sparse,
    LocalDistinctiveness,
    CoverageLocal,
    CoverageTeam,
    TeamCoverageLocal,
    TypedEvidenceAdditive,
    GatedCoverageLocal,
    GatedCoverageTeam,
}

impl SegmentRoutingArg {
    fn strategy(self) -> SegmentRoutingStrategy {
        match self {
            SegmentRoutingArg::Sparse => SegmentRoutingStrategy::SparseOverlap,
            SegmentRoutingArg::LocalDistinctiveness => SegmentRoutingStrategy::LocalDistinctiveness,
            SegmentRoutingArg::CoverageLocal => {
                SegmentRoutingStrategy::CoverageLocalDistinctiveness
            }
            SegmentRoutingArg::CoverageTeam => SegmentRoutingStrategy::CoverageTeamSelection,
            SegmentRoutingArg::TeamCoverageLocal => {
                SegmentRoutingStrategy::TeamCoverageLocalDistinctiveness
            }
            SegmentRoutingArg::TypedEvidenceAdditive => SegmentRoutingStrategy::TypedEvidence,
            SegmentRoutingArg::GatedCoverageLocal => {
                SegmentRoutingStrategy::TypedEvidenceMultiplicative
            }
            SegmentRoutingArg::GatedCoverageTeam => {
                SegmentRoutingStrategy::CoverageTeamTypedMultiplicative
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    service: Arc<RwLock<MemoryService>>,
    writer_gate: Arc<tokio::sync::Mutex<()>>,
    staged_writer: Option<StagedWriter>,
    token: Option<Arc<str>>,
    jwt_secret: Option<Arc<str>>,
    tenant_id: Option<Arc<str>>,
    telemetry: OperationalTelemetry,
    project_root: PathBuf,
    workspace_index_root: PathBuf,
    workspace_watcher: Option<Arc<crate::pipeline::WorkspaceWatcher>>,
    workspace_registry: Arc<WorkspaceRegistry>,
}

impl AppState {
    fn provider_published_view(&self) -> anyhow::Result<Arc<MemoryService>> {
        self.staged_writer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("resident memory writer is unavailable"))?
            .read_view()
    }
}

#[derive(Clone)]
struct WorkspaceRuntime {
    #[cfg(any(feature = "claude-code", feature = "codex"))]
    transcript_captures: Arc<crate::memory_api::transcript_capture::TranscriptCaptures>,
    service: Arc<RwLock<MemoryService>>,
    staged_writer: StagedWriter,
    writer_gate: Arc<tokio::sync::Mutex<()>>,
    _owner_lock: Arc<crate::memory_api::workspace::WorkspaceOwnerLock>,
    project_root: PathBuf,
    workspace_index_root: PathBuf,
}

struct WorkspaceRegistry {
    options: PipelineOptions,
    schedule: server_writer::WriteSchedule,
    allowed_roots: HashSet<PathBuf>,
    max_active_workspaces: usize,
    state: tokio::sync::Mutex<WorkspaceRegistryState>,
}

#[derive(Debug)]
enum WorkspaceResolveError {
    InvalidRoot,
    NotAllowed,
    Capacity,
    Initialization,
}

impl IntoResponse for WorkspaceResolveError {
    fn into_response(self) -> axum::response::Response {
        let (status, detail) = match self {
            Self::InvalidRoot => (StatusCode::FORBIDDEN, "invalid workspace root"),
            Self::NotAllowed => (StatusCode::FORBIDDEN, "workspace root is not allowed"),
            Self::Capacity => (StatusCode::TOO_MANY_REQUESTS, "workspace capacity reached"),
            Self::Initialization => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "workspace initialization failed",
            ),
        };
        (status, Json(json!({"detail": detail}))).into_response()
    }
}

#[derive(Default)]
struct WorkspaceRegistryState {
    entries: HashMap<PathBuf, Arc<WorkspaceRuntime>>,
    opening: HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>,
}

impl WorkspaceRegistry {
    fn new(
        options: PipelineOptions,
        schedule: server_writer::WriteSchedule,
        allowed_roots: HashSet<PathBuf>,
        max_active_workspaces: usize,
    ) -> Self {
        Self {
            options,
            schedule,
            allowed_roots,
            max_active_workspaces,
            state: tokio::sync::Mutex::new(WorkspaceRegistryState::default()),
        }
    }

    fn register_primary(&self, runtime: WorkspaceRuntime) {
        // Inserted before the router is served, so this blocking lock is only
        // used during startup.
        let mut state = self.state.try_lock().expect("registry is not serving yet");
        state
            .entries
            .insert(runtime.project_root.clone(), Arc::new(runtime));
    }

    async fn release_failed_opening(&self, root: &PathBuf, opening: &Arc<tokio::sync::Mutex<()>>) {
        let mut state = self.state.lock().await;
        let is_last_waiter = state.opening.get(root).is_some_and(|registered| {
            Arc::ptr_eq(registered, opening) && Arc::strong_count(opening) == 2
        });
        if is_last_waiter {
            state.opening.remove(root);
        }
    }

    async fn resolve(
        &self,
        requested: &str,
    ) -> Result<Arc<WorkspaceRuntime>, WorkspaceResolveError> {
        let root =
            std::fs::canonicalize(requested).map_err(|_| WorkspaceResolveError::InvalidRoot)?;
        if !root.is_dir() {
            return Err(WorkspaceResolveError::InvalidRoot);
        }
        if !self.allowed_roots.contains(&root) {
            return Err(WorkspaceResolveError::NotAllowed);
        }
        let opening = {
            let mut state = self.state.lock().await;
            if let Some(runtime) = state.entries.get(&root) {
                return Ok(runtime.clone());
            }
            if let Some(opening) = state.opening.get(&root) {
                opening.clone()
            } else {
                if state.entries.len() + state.opening.len() >= self.max_active_workspaces {
                    return Err(WorkspaceResolveError::Capacity);
                }
                let opening = Arc::new(tokio::sync::Mutex::new(()));
                state.opening.insert(root.clone(), opening.clone());
                opening
            }
        };
        // Requests for this workspace share a single initializer, while
        // requests for already-open or different workspaces remain available.
        let _opening = opening.lock().await;
        {
            let state = self.state.lock().await;
            if let Some(runtime) = state.entries.get(&root) {
                return Ok(runtime.clone());
            }
        }

        // Serialize first-open for a workspace so simultaneous MCP startup and
        // hook requests cannot create two writers for the same on-disk stores.
        let options = self.options.clone();
        let schedule = self.schedule;
        let runtime_root = root.clone();
        let runtime = match tokio::task::spawn_blocking(move || {
            open_workspace_runtime(runtime_root, options, schedule)
        })
        .await
        {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!("workspace initialization task failed: {error}");
                self.release_failed_opening(&root, &opening).await;
                return Err(WorkspaceResolveError::Initialization);
            }
        };
        let runtime = match runtime {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                tracing::error!("workspace initialization failed: {error:#}");
                self.release_failed_opening(&root, &opening).await;
                return Err(WorkspaceResolveError::Initialization);
            }
        };
        start_runtime_workspace_watcher(runtime.clone());
        let mut state = self.state.lock().await;
        state.entries.insert(root.clone(), runtime.clone());
        state.opening.remove(&root);
        Ok(runtime)
    }
}

#[cfg(any(feature = "claude-code", feature = "codex"))]
fn start_transcript_captures(
    root: &std::path::Path,
    writer: &StagedWriter,
    owner_lock: &Arc<crate::memory_api::workspace::WorkspaceOwnerLock>,
) -> anyhow::Result<Arc<crate::memory_api::transcript_capture::TranscriptCaptures>> {
    let writer = writer.clone();
    let owner_lock = owner_lock.clone();
    Ok(Arc::new(MemoryService::start_transcript_captures(
        root,
        move |request, documents| {
            let _keep_owner = &owner_lock;
            let audit = server_writer::CaptureAudit {
                provider: request.provider.clone(),
                user_id: "mcp".into(),
                session_id: request.input["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            };
            writer.upsert_documents_blocking_with_audit(documents, Some(audit))
        },
    )?))
}

fn transcript_capture_status(runtime: &WorkspaceRuntime) -> Value {
    #[cfg(any(feature = "claude-code", feature = "codex"))]
    {
        return match runtime.transcript_captures.status() {
            Ok(status) => json!(status),
            Err(error) => json!({"error":error.to_string()}),
        };
    }
    #[cfg(not(any(feature = "claude-code", feature = "codex")))]
    {
        let _ = runtime;
        Value::Null
    }
}

fn open_workspace_runtime(
    project_root: PathBuf,
    options: PipelineOptions,
    schedule: server_writer::WriteSchedule,
) -> anyhow::Result<WorkspaceRuntime> {
    let owner_lock = crate::memory_api::workspace::WorkspaceOwnerLock::acquire(&project_root)?;
    let workspace_index_root = project_root
        .join(".membrick")
        .join(crate::memory_api::workspace::WORKSPACE_MEMORY_NAME);
    let owner = open_workspace_owner_at(&project_root, &workspace_index_root, options.clone())?;
    let service = Arc::new(RwLock::new(owner));
    let staged_writer = StagedWriter::start_with_schedule(service.clone(), schedule)?;
    let owner_lock = Arc::new(owner_lock);
    let runtime = WorkspaceRuntime {
        #[cfg(any(feature = "claude-code", feature = "codex"))]
        transcript_captures: start_transcript_captures(&project_root, &staged_writer, &owner_lock)?,
        service,
        staged_writer,
        writer_gate: Arc::new(tokio::sync::Mutex::new(())),
        _owner_lock: owner_lock,
        project_root,
        workspace_index_root,
    };
    Ok(runtime)
}

fn start_runtime_workspace_watcher(runtime: Arc<WorkspaceRuntime>) {
    let watcher = match crate::pipeline::WorkspaceWatcher::new(
        &runtime.project_root,
        &provider_memory_ignore_paths(),
    ) {
        Ok(watcher) => Arc::new(watcher),
        Err(error) => {
            tracing::warn!(
                "workspace watcher unavailable for {}: {error:#}",
                runtime.project_root.display()
            );
            return;
        }
    };
    if let Ok(service) = runtime.service.read() {
        watcher.seed_baseline(
            service
                .memory_store(MemoryStore::Workspace)
                .source_documents()
                .iter()
                .map(|document| (document.source.as_str(), document.content.as_str())),
        );
    }
    start_runtime_watcher_with_handle((*runtime).clone(), watcher);
}

fn start_runtime_watcher_with_handle(
    runtime: WorkspaceRuntime,
    watcher: Arc<crate::pipeline::WorkspaceWatcher>,
) {
    let root = runtime.project_root.clone();
    let index_root = runtime.workspace_index_root.clone();
    let writer = runtime.staged_writer.for_workspace();
    let ignore_paths = provider_memory_ignore_paths();
    tokio::spawn(async move {
        let mut poll = tokio::time::interval(Duration::from_millis(500));
        let mut refresh_pending = false;
        let mut retry_after = None;
        loop {
            poll.tick().await;
            if watcher
                .take_file_changes()
                .iter()
                .any(|change| change.affects_workspace_index(&root))
            {
                refresh_pending = true;
            }
            if !refresh_pending
                || retry_after.is_some_and(|deadline| std::time::Instant::now() < deadline)
            {
                continue;
            }
            let build_root = root.clone();
            let build_ignores = ignore_paths.clone();
            let documents = tokio::task::spawn_blocking(move || {
                let input = crate::adapters::AdapterInput {
                    root: &build_root,
                    max_bytes: 5_000_000,
                    max_files: 50_000,
                    max_depth: 20,
                    max_total_bytes: 100_000_000,
                };
                let graph = crate::adapters::build_project_graph(&input)?;
                let graph = crate::adapters::apply_ignore_paths(graph, &build_ignores);
                Ok::<_, anyhow::Error>(crate::adapters::graph_to_source_documents(&graph))
            })
            .await;
            let result = match documents {
                Ok(Ok(documents)) => writer
                    .replace_workspace_documents(documents)
                    .await
                    .map_err(|status| anyhow::anyhow!("workspace write returned {status}"))
                    .and_then(|()| {
                        crate::memory_api::workspace::persist_workspace_index_state(
                            &root,
                            &index_root,
                            &ignore_paths,
                        )
                    }),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
            match result {
                Ok(()) => {
                    refresh_pending = false;
                    retry_after = None;
                }
                Err(error) => {
                    tracing::warn!("workspace refresh failed for {}: {error:#}", root.display());
                    retry_after = Some(std::time::Instant::now() + Duration::from_secs(2));
                }
            }
        }
    });
}

#[derive(serde::Serialize)]
struct DashboardStatus {
    status: &'static str,
    version: &'static str,
    uptime_seconds: u64,
    index: DashboardIndexStatus,
    telemetry: TelemetrySnapshot,
    integrations: Vec<DashboardIntegrationStatus>,
    transcript_captures: Value,
}

#[derive(serde::Serialize)]
struct DashboardIndexStatus {
    source_document_count: usize,
    record_count: usize,
    dirty: bool,
    store_revision: u64,
    snapshot_revision: u64,
    revision_lag: u64,
    snapshot: Option<DashboardSnapshotStatus>,
}

#[derive(serde::Serialize)]
struct DashboardSnapshotStatus {
    layout: String,
    segment_count: usize,
    global_document_count: usize,
    segments: Vec<DashboardSegmentStatus>,
}

#[derive(serde::Serialize)]
struct DashboardSegmentStatus {
    segment_id: String,
    document_count: usize,
    profile_term_count: usize,
    profile_entity_count: usize,
    profile_topic_count: usize,
    profile_local_memory_count: usize,
}

#[derive(serde::Serialize)]
struct DashboardIntegrationStatus {
    provider: &'static str,
    #[serde(skip_serializing)]
    recording_provider: &'static str,
    compiled: bool,
    state: &'static str,
    note: &'static str,
    last_event: Option<String>,
    last_seen_ms: Option<u64>,
    events_total: u64,
    sessions_started: u64,
    sessions_ended: u64,
    sessions_active: u64,
    retrieval_events: u64,
    capture_events: u64,
    indexes: Vec<DashboardProviderIndex>,
    events: Vec<ProviderLifecycleEvent>,
    /// Retention cap of `events`, so clients can label window-only figures.
    event_window_limit: usize,
    /// Cumulative counters, or `None` when the ledger predates them.
    counters: Option<DashboardProviderCounters>,
    /// The newest prompt, stop, usage and subagent events.
    milestone_events: Vec<ProviderLifecycleEvent>,
}

/// Cumulative per-provider counters as served by `/api/status`. The ledger's
/// open-prompt bookkeeping is not exposed.
#[derive(serde::Serialize)]
struct DashboardProviderCounters {
    since_ms: u64,
    prompts: u64,
    stops: u64,
    tool_calls: u64,
    tool_results: u64,
    tool_failures: u64,
    subagent_starts: u64,
    subagent_stops: u64,
    usage_events: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    reported_total_tokens: u64,
    cycles_completed: u64,
    cycle_duration_sum_ms: u64,
    cycle_duration_max_ms: u64,
    cycle_duration_avg_ms: Option<u64>,
    cycle_duration_p50_ms: Option<u64>,
    cycle_duration_p95_ms: Option<u64>,
    subagent_interrupted: u64,
    subagent_ended_without_stop: u64,
    /// Subagents started and not yet stopped, by agent id.
    open_subagents: std::collections::BTreeMap<String, crate::telemetry::OpenSubagent>,
}

impl DashboardProviderCounters {
    fn from_ledger(counters: &crate::telemetry::ProviderLifecycleCounters) -> Option<Self> {
        if counters.since_ms == 0 {
            return None;
        }
        Some(Self {
            since_ms: counters.since_ms,
            prompts: counters.prompts,
            stops: counters.stops,
            tool_calls: counters.tool_calls,
            tool_results: counters.tool_results,
            tool_failures: counters.tool_failures,
            subagent_starts: counters.subagent_starts,
            subagent_stops: counters.subagent_stops,
            usage_events: counters.usage_events,
            input_tokens: counters.input_tokens,
            output_tokens: counters.output_tokens,
            cache_creation_input_tokens: counters.cache_creation_input_tokens,
            cache_read_input_tokens: counters.cache_read_input_tokens,
            reported_total_tokens: counters.reported_total_tokens,
            cycles_completed: counters.cycles_completed,
            cycle_duration_sum_ms: counters.cycle_duration_sum_ms,
            cycle_duration_max_ms: counters.cycle_duration_max_ms,
            cycle_duration_avg_ms: (counters.cycles_completed > 0)
                .then(|| counters.cycle_duration_sum_ms / counters.cycles_completed),
            cycle_duration_p50_ms: counters.cycle_duration_quantile_ms(0.50),
            cycle_duration_p95_ms: counters.cycle_duration_quantile_ms(0.95),
            subagent_interrupted: counters.subagent_interrupted,
            subagent_ended_without_stop: counters.subagent_ended_without_stop,
            open_subagents: counters.open_subagents.clone(),
        })
    }
}

#[derive(serde::Serialize)]
struct DashboardProviderIndex {
    name: String,
    role: String,
    source_document_count: usize,
    record_count: usize,
    dirty: bool,
    store_revision: u64,
    snapshot_revision: u64,
    snapshot: Option<DashboardSnapshotStatus>,
}

#[derive(serde::Serialize)]
struct DashboardSession {
    provider: String,
    session_key: String,
    event_count: usize,
    first_seen_ms: u64,
    last_seen_ms: u64,
    last_event: String,
}

fn provider_memory_ignore_paths() -> Vec<String> {
    [
        "node_modules",
        "target",
        "dist",
        "build",
        "vendor",
        "coverage",
        ".git",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[cfg(test)]
fn open_workspace_owner(root: &std::path::Path) -> anyhow::Result<MemoryService> {
    let workspace_root = root
        .join(".membrick")
        .join(crate::memory_api::workspace::WORKSPACE_MEMORY_NAME);
    open_workspace_owner_at(
        root,
        &workspace_root,
        crate::memory_api::workspace::segmented_store_options(),
    )
}

fn open_workspace_owner_at(
    root: &std::path::Path,
    workspace_root: &std::path::Path,
    options: PipelineOptions,
) -> anyhow::Result<MemoryService> {
    let ignore_paths = provider_memory_ignore_paths();
    let mut owner = MemoryService::open_workspace_owner_with_options_at(
        root,
        workspace_root,
        crate::integrations::mcp_index::SHARED_MEMORY_DIR,
        &ignore_paths,
        options,
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
    )?;
    // Use the scope extension routes use, so adopted records show up there.
    match crate::integrations::mcp_tools::adopt_legacy_extension_records(
        &mut owner,
        crate::MCP_MEMORY_USER_ID,
        &root.to_string_lossy(),
        &root.join(".membrick").join("migrations"),
    ) {
        Ok(adoption) if adoption.adopted > 0 => tracing::info!(
            adopted = adoption.adopted,
            left_in_place = adoption.left_in_place,
            backup = ?adoption.backup,
            "moved legacy records into extension storage"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!("legacy extension records were not adopted: {error:#}"),
    }
    owner.start_search_history_index(root);
    Ok(owner)
}

#[tokio::main]
pub(crate) async fn main() -> anyhow::Result<()> {
    // Declare this first so Rust drops it after server state, writer workers,
    // and workspace ownership locks during the function's teardown.
    let mut _resident_shutdown_gate = None;
    let mut matches = Args::command().get_matches();
    let bind_was_set = matches.value_source("bind") == Some(clap::parser::ValueSource::CommandLine);
    let mut args = Args::from_arg_matches_mut(&mut matches)?;
    let settings_file = settings_path(&args)?;
    let settings = load_server_settings(&settings_file)?;
    let resident_http_bind = settings.resident_http_bind.clone();
    let resident_http_auto_port = settings.resident_http_auto_port.unwrap_or(false);
    let resident_idle_timeout = std::time::Duration::from_secs(
        settings
            .resident_idle_timeout_seconds
            .unwrap_or(60)
            .clamp(5, 3600),
    );
    if !bind_was_set {
        if let Some(bind) = settings.bind {
            args.bind = bind;
        }
    }
    if args.index.is_none() {
        args.index = settings.index;
    }
    if args.resident_server {
        // A resident owns exactly one installed project root and never
        // creates state for any other folder.
        let root = match &args.project_root {
            Some(root) => root.clone(),
            None => std::env::current_dir()?,
        };
        args.project_root = Some(crate::state_paths::validate_project_root(&root)?);
    } else if args.project_root.is_none() {
        args.project_root = settings.project_root;
    }
    if args.server_token.is_none() {
        args.server_token = if let Some(path) = settings.server_token_file {
            Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read token file {}", path.display()))?
                    .trim()
                    .to_string(),
            )
        } else {
            settings.server_token
        };
    }
    if args.tenant_id.is_none() {
        args.tenant_id = settings.tenant_id;
    }
    if args.adaptive_segment_max_n.is_none() {
        args.adaptive_segment_max_n = settings.adaptive_segment_max_n;
    }
    let bekind_enabled = args.bekind;
    crate::behood_query::set_enabled(bekind_enabled);
    args.server_token = normalize_secret(args.server_token);
    let jwt_secret = normalize_secret(settings.jwt_secret);
    ensure_bind_allowed(
        &args.bind,
        args.allow_non_loopback,
        args.server_token.is_some() || jwt_secret.is_some(),
        args.allow_unauthenticated,
    )?;
    if args.resident_server {
        if let Some(bind) = &resident_http_bind {
            ensure_bind_allowed(
                bind,
                args.allow_non_loopback,
                args.server_token.is_some() || jwt_secret.is_some(),
                args.allow_unauthenticated,
            )?;
        }
    }
    let mut options = memory_pipeline_options(
        args.adaptive_segment_max_n,
        args.single_index,
        args.global_index,
        args.fuse_global,
        !args.no_conversational_rerank,
        args.segment_routing.strategy(),
    );
    options.freshness_policy = settings
        .freshness
        .unwrap_or_default()
        .validate()
        .map_err(anyhow::Error::msg)?;
    options.lang = args.lang;
    let project_root = args
        .project_root
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    // The effective settings after command-line flags and server.toml are
    // merged, with where this process started from. Secrets are reported
    // only as set or unset.
    tracing::debug!(
        startup_dir = %std::env::current_dir()?.display(),
        executable = %std::env::current_exe()?.display(),
        settings_file = %settings_file.display(),
        project_root = %project_root.display(),
        resident_server = args.resident_server,
        managed_resident = args.managed_resident,
        bind = %args.bind,
        resident_http_bind = ?resident_http_bind,
        resident_http_auto_port,
        resident_idle_timeout_seconds = resident_idle_timeout.as_secs(),
        index = ?args.index,
        tenant_id = ?args.tenant_id,
        adaptive_segment_max_n = ?args.adaptive_segment_max_n,
        freshness = ?options.freshness_policy,
        server_token = if args.server_token.is_some() { "set" } else { "unset" },
        jwt_secret = if jwt_secret.is_some() { "set" } else { "unset" },
        "server settings"
    );
    let owner_lock = crate::memory_api::workspace::WorkspaceOwnerLock::acquire(&project_root)?;
    anyhow::ensure!(
        args.max_active_workspaces > 0,
        "--max-active-workspaces must be greater than zero"
    );
    let mut allowed_workspace_roots = HashSet::from([project_root.clone()]);
    for root in args.workspace_roots {
        let root = root.canonicalize()?;
        anyhow::ensure!(root.is_dir(), "workspace root is not a directory");
        allowed_workspace_roots.insert(root);
    }
    let discovered_indexes = discover_index_paths(&project_root);
    // behood (bekind) owns the full tag→chunk→judge pipeline (Luyi
    // 2026-09-30): the query path sends raw texts to `bekind --serve` and
    // gets verdicts back. No parse backend to select, no spaCy involved.
    // Note: the structured-relations extractor (ExtractorDaemon,
    // scripts/spacy_relations.py) is a separate spaCy component.
    let selected_index = args.index.clone().or_else(|| {
        discovered_indexes
            .iter()
            .find(|path| {
                path.file_name().and_then(|name| name.to_str()) == Some("workspace-memory")
            })
            .cloned()
            // Keep existing installations usable during their migration. New
            // provider MCP servers create workspace-memory instead.
            .or_else(|| {
                discovered_indexes
                    .iter()
                    .find(|path| {
                        path.file_name().and_then(|name| name.to_str()) == Some("codex-mcp-index")
                    })
                    .cloned()
            })
            .or_else(|| {
                discovered_indexes
                    .iter()
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.ends_with("-mcp-index"))
                    })
                    .cloned()
            })
    });
    let workspace_index_root =
        selected_index.unwrap_or_else(|| project_root.join(".membrick").join("workspace-memory"));
    let owner = open_workspace_owner_at(&project_root, &workspace_index_root, options.clone())?;
    let ignore_paths = provider_memory_ignore_paths();
    let watcher = crate::pipeline::WorkspaceWatcher::new(&project_root, &ignore_paths)?;
    {
        let documents = owner
            .memory_store(MemoryStore::Workspace)
            .source_documents();
        watcher.seed_baseline(
            documents
                .iter()
                .map(|document| (document.source.as_str(), document.content.as_str())),
        );
    }
    let workspace_watcher = Some(Arc::new(watcher));
    let write_schedule = server_writer::WriteSchedule {
        refresh_interval: Duration::from_millis(args.refresh_interval_ms),
        batch_size: args.refresh_batch_size,
        checkpoint_interval: Duration::from_secs(args.checkpoint_interval_seconds),
    };
    let workspace_registry = Arc::new(WorkspaceRegistry::new(
        options.clone(),
        write_schedule,
        allowed_workspace_roots,
        args.max_active_workspaces,
    ));
    let mut state = AppState {
        service: Arc::new(RwLock::new(owner)),
        writer_gate: Arc::new(tokio::sync::Mutex::new(())),
        staged_writer: None,
        token: args
            .server_token
            .map(|s| Arc::<str>::from(s.trim().to_owned())),
        jwt_secret: jwt_secret.map(Arc::<str>::from),
        tenant_id: args
            .tenant_id
            .map(|s| Arc::<str>::from(s.trim().to_owned())),
        telemetry: OperationalTelemetry::new(),
        project_root,
        workspace_index_root,
        workspace_watcher,
        workspace_registry: workspace_registry.clone(),
    };
    // Warm the Python daemon children in the background: the first query
    // that needs key-phrase backfill or structured relations then pays
    // inference only (~100ms) instead of interpreter+model load (~2-3s).
    // Best-effort — extraction/analysis falls back to one-shot subprocesses
    // if a daemon cannot start.
    //
    // The NER daemon is deliberately NOT prewarmed: it starts lazily on the
    // first NER request. Prewarming would force a third Python+spaCy child
    // (~145MB RSS) on every server start, even when the heuristic NER
    // provider is configured and spaCy is never used. (Luyi 2026-09-29 P2.)
    std::thread::Builder::new()
        .name("python-daemon-prewarm".to_string())
        .spawn(move || {
            crate::segments::extractor_daemon::ExtractorDaemon::global().prewarm();
            // The judge daemon is optional and prewarms only when --bekind
            // enabled it.
            if bekind_enabled {
                crate::behood_query::BekindDaemon::global().prewarm();
            }
        })
        .ok();
    let writer = StagedWriter::start_with_schedule(state.service.clone(), write_schedule)?;
    state.staged_writer = Some(writer);
    let owner_lock = Arc::new(owner_lock);
    workspace_registry.register_primary(WorkspaceRuntime {
        #[cfg(any(feature = "claude-code", feature = "codex"))]
        transcript_captures: start_transcript_captures(
            &state.project_root,
            state.staged_writer.as_ref().unwrap(),
            &owner_lock,
        )?,
        service: state.service.clone(),
        staged_writer: state.staged_writer.as_ref().unwrap().clone(),
        writer_gate: state.writer_gate.clone(),
        _owner_lock: owner_lock,
        project_root: state.project_root.clone(),
        workspace_index_root: state.workspace_index_root.clone(),
    });
    if let (Some(watcher), Some(workspace_writer)) = (
        state.workspace_watcher.clone(),
        state
            .staged_writer
            .as_ref()
            .map(StagedWriter::for_workspace),
    ) {
        let project_root = state.project_root.clone();
        let workspace_index_root = state.workspace_index_root.clone();
        let ignore_paths = provider_memory_ignore_paths();
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_millis(500));
            let mut refresh_pending = false;
            let mut retry_after = None;
            loop {
                poll.tick().await;
                if watcher
                    .take_file_changes()
                    .iter()
                    .any(|change| change.affects_workspace_index(&project_root))
                {
                    refresh_pending = true;
                }
                if !refresh_pending
                    || retry_after.is_some_and(|deadline| std::time::Instant::now() < deadline)
                {
                    continue;
                }
                let build_root = project_root.clone();
                let build_ignores = ignore_paths.clone();
                let documents = tokio::task::spawn_blocking(move || {
                    let input = crate::adapters::AdapterInput {
                        root: &build_root,
                        max_bytes: 5_000_000,
                        max_files: 50_000,
                        max_depth: 20,
                        max_total_bytes: 100_000_000,
                    };
                    let graph = crate::adapters::build_project_graph(&input)?;
                    let graph = crate::adapters::apply_ignore_paths(graph, &build_ignores);
                    Ok::<_, anyhow::Error>(crate::adapters::graph_to_source_documents(&graph))
                })
                .await;
                let result = match documents {
                    Ok(Ok(documents)) => workspace_writer
                        .replace_workspace_documents(documents)
                        .await
                        .map_err(|status| anyhow::anyhow!("workspace write returned {status}"))
                        .and_then(|()| {
                            crate::memory_api::workspace::persist_workspace_index_state(
                                &project_root,
                                &workspace_index_root,
                                &ignore_paths,
                            )
                        }),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                };
                match result {
                    Ok(()) => {
                        refresh_pending = false;
                        retry_after = None;
                    }
                    Err(error) => {
                        tracing::warn!("workspace index refresh failed: {error:#}");
                        retry_after = Some(std::time::Instant::now() + Duration::from_secs(5));
                    }
                }
            }
        });
    }
    let app = app_router(state.clone());
    if args.resident_server {
        let root = state.project_root.clone();
        let managed = args.managed_resident;
        let idle_timeout = resident_idle_timeout;
        let (log_level, log_level_source) = crate::logging::active_level()
            .map_or(("unset".to_owned(), "none"), |(level, source)| {
                (level.to_string(), source)
            });
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            managed,
            root = %state.project_root.display(),
            log_level,
            log_level_source,
            "resident starting"
        );
        let shutdown = async move {
            crate::local_ipc::resident_shutdown(&root, managed, idle_timeout, shutdown_signal())
                .await
        };
        _resident_shutdown_gate = crate::local_ipc::serve(
            app,
            &state.project_root,
            shutdown,
            resident_http_bind.as_deref(),
            resident_http_auto_port,
            Some(&settings_file),
        )
        .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&args.bind).await?;
        tracing::info!("Membrick server listening on {}", args.bind);
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
    }
    #[cfg(any(feature = "claude-code", feature = "codex"))]
    {
        let captures = state
            .workspace_registry
            .state
            .lock()
            .await
            .entries
            .values()
            .map(|runtime| runtime.transcript_captures.clone())
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            for worker in captures {
                worker.shutdown();
            }
        })
        .await?;
    }
    if let Some(worker) = &state.staged_writer {
        worker
            .flush()
            .await
            .map_err(|status| anyhow::anyhow!("shutdown flush failed: {status}"))?;
    }
    crate::extension_host::shutdown()
        .map_err(|error| anyhow::anyhow!("extension shutdown failed: {error}"))?;
    tracing::info!("shutdown complete; pending writes flushed");
    Ok(())
}

/// Completes on Ctrl-C or SIGTERM, so either listener can stop and flush.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn app_router(state: AppState) -> Router {
    let router = Router::new()
        .merge(server_operations_routes::router())
        .merge(server_dashboard_routes::router())
        .merge(server_extension_events::router())
        .merge(server_memory_routes::router())
        .merge(server_provider_routes::router())
        .fallback(any(extension_http_route));
    router
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_: BoxError| async {
                    (
                        StatusCode::REQUEST_TIMEOUT,
                        Json(json!({"detail": "request timed out"})),
                    )
                }))
                .layer(tower::timeout::TimeoutLayer::new(Duration::from_secs(30)))
                .layer(tower::limit::ConcurrencyLimitLayer::new(
                    MAX_CONCURRENT_REQUESTS,
                )),
        )
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn extension_http_route(
    State(state): State<AppState>,
    request: Request<Body>,
) -> axum::response::Response {
    use membrick_extension_api::ExtensionHttpRequest;

    let Some(method) = extension_http_method(request.method()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let registry = crate::extension_host::registry();
    let Some(route) = registry.match_http_route(method, request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) if body.is_empty() => Value::Null,
        Ok(body) => match serde_json::from_slice::<Value>(&body) {
            Ok(body) => body,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": format!("invalid JSON request body: {error}")})),
                )
                    .into_response()
            }
        },
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": format!("could not read request body: {error}")})),
            )
                .into_response()
        }
    };
    let extension_request = ExtensionHttpRequest {
        path_params: BTreeMap::new(),
        query: extension_query_parameters(parts.uri.query()),
        body,
    };
    let workspace = state.project_root.to_string_lossy().into_owned();

    // Extension route metadata is descriptive only. A handler can mutate
    // storage even when it labels a route read-only, so every extension route
    // uses the host's serialized durable path.
    let response = if let Some(writer) = state.staged_writer.clone() {
        let route_for_call = route.clone();
        let workspace_for_call = workspace.clone();
        match writer
            .mutate(move |service| {
                Ok(extension_http_result_value(
                    crate::integrations::mcp_tools::dispatch_extension_http_route(
                        &route_for_call,
                        extension_request,
                        service,
                        crate::MCP_MEMORY_USER_ID,
                        &workspace_for_call,
                        "http",
                    ),
                ))
            })
            .await
        {
            Ok(value) => extension_http_result_from_value(value),
            Err(error) => {
                tracing::warn!("extension HTTP invocation failed: {error:#}");
                extension_http_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "extension writer unavailable",
                )
            }
        }
    } else {
        extension_http_dispatch_locked(&state, &route, extension_request, &workspace)
    };
    extension_http_into_response(response)
}

fn extension_http_method(method: &Method) -> Option<membrick_extension_api::HttpMethod> {
    use membrick_extension_api::HttpMethod;
    match *method {
        Method::GET => Some(HttpMethod::Get),
        Method::POST => Some(HttpMethod::Post),
        Method::PUT => Some(HttpMethod::Put),
        Method::PATCH => Some(HttpMethod::Patch),
        Method::DELETE => Some(HttpMethod::Delete),
        Method::HEAD => Some(HttpMethod::Head),
        Method::OPTIONS => Some(HttpMethod::Options),
        _ => None,
    }
}

fn extension_query_parameters(query: Option<&str>) -> BTreeMap<String, Vec<String>> {
    let mut parsed = BTreeMap::<String, Vec<String>>::new();
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        parsed
            .entry(extension_query_decode(key))
            .or_default()
            .push(extension_query_decode(value));
    }
    parsed
}

fn extension_query_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = |byte: u8| match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                };
                match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        decoded.push((high << 4) | low);
                        index += 3;
                    }
                    _ => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn extension_http_dispatch_locked(
    state: &AppState,
    route: &membrick_extension_api::ExtensionRouteMatch,
    request: membrick_extension_api::ExtensionHttpRequest,
    workspace: &str,
) -> membrick_extension_api::ExtensionHttpResponse {
    match state.service.write() {
        Ok(mut service) => extension_http_result_from_value(extension_http_result_value(
            crate::integrations::mcp_tools::dispatch_extension_http_route(
                route,
                request,
                &mut service,
                crate::MCP_MEMORY_USER_ID,
                workspace,
                "http",
            ),
        )),
        Err(_) => extension_http_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory service is unavailable",
        ),
    }
}

fn extension_http_result_value(
    result: Result<
        membrick_extension_api::ExtensionHttpResponse,
        membrick_extension_api::ExtensionError,
    >,
) -> Value {
    use membrick_extension_api::ExtensionErrorKind;
    match result {
        Ok(response) => json!({
            "status": response.status,
            "headers": response.headers,
            "body": response.body,
        }),
        Err(error) => {
            let status = match error.kind() {
                ExtensionErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                ExtensionErrorKind::NotFound => StatusCode::NOT_FOUND,
                ExtensionErrorKind::Conflict => StatusCode::CONFLICT,
                ExtensionErrorKind::Forbidden => StatusCode::FORBIDDEN,
                ExtensionErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                ExtensionErrorKind::Unsupported => StatusCode::NOT_IMPLEMENTED,
                ExtensionErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            json!({ "status": status.as_u16(), "headers": [], "body": {"detail": error.message()} })
        }
    }
}

fn extension_http_result_from_value(value: Value) -> membrick_extension_api::ExtensionHttpResponse {
    let status = value["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
    let headers = value["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|header| {
            Some((
                header.get(0)?.as_str()?.to_owned(),
                header.get(1)?.as_str()?.to_owned(),
            ))
        })
        .collect();
    membrick_extension_api::ExtensionHttpResponse {
        status,
        headers,
        body: value.get("body").cloned().unwrap_or(Value::Null),
    }
}

fn extension_http_error_response(
    status: StatusCode,
    detail: &str,
) -> membrick_extension_api::ExtensionHttpResponse {
    membrick_extension_api::ExtensionHttpResponse::json(status.as_u16(), json!({"detail": detail}))
}

fn extension_http_into_response(
    response: membrick_extension_api::ExtensionHttpResponse,
) -> axum::response::Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = match serde_json::to_vec(&response.body) {
        Ok(body) => body,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": format!("invalid extension response: {error}")})),
            )
                .into_response()
        }
    };
    let mut builder = axum::response::Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    for (name, value) in response.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":"extension returned an invalid HTTP header"})),
            )
                .into_response();
        };
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(feature = "openclaw")]

/// Production pipeline options with the server's CLI flags applied as
/// overrides on top of [`default_production_pipeline_options`].
fn memory_pipeline_options(
    adaptive_segment_max_n: Option<usize>,
    single_index: bool,
    global_index: bool,
    fuse_global: bool,
    conversational_rerank: bool,
    routing_strategy: SegmentRoutingStrategy,
) -> PipelineOptions {
    if single_index {
        return PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Single,

            ..default_production_pipeline_options()
        };
    }
    let layout = if global_index {
        MemoryIndexLayout::Segmented {
            query_top_n: usize::MAX,
            routing_strategy,
        }
    } else {
        adaptive_segment_max_n
            .filter(|max_n| *max_n > DEFAULT_SEGMENT_QUERY_TOP_N)
            .map(|max_n| MemoryIndexLayout::AdaptiveSegmented {
                query_top_n: DEFAULT_SEGMENT_QUERY_TOP_N,
                max_query_n: max_n,
                routing_strategy,
            })
            .unwrap_or(MemoryIndexLayout::Segmented {
                query_top_n: DEFAULT_SEGMENT_QUERY_TOP_N,
                routing_strategy,
            })
    };
    PipelineOptions {
        memory_index_layout: layout,
        fuse_global_arm: fuse_global,
        conversational_rerank,

        ..default_production_pipeline_options()
    }
}

fn normalize_secret(value: Option<String>) -> Option<String> {
    value
        .map(|secret| secret.trim().to_owned())
        .filter(|secret| !secret.is_empty())
}

fn browser_request_allowed(headers: &HeaderMap, authenticated_server: bool) -> bool {
    let authority = headers.get("host").and_then(|value| value.to_str().ok());
    let host_url = authority
        .and_then(|authority| reqwest::Url::parse(&format!("http://{authority}/")).ok())
        .filter(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
        });
    if headers.contains_key("host") && host_url.is_none() {
        return false;
    }
    if !authenticated_server {
        if let Some(url) = host_url.as_ref() {
            let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
            if host != "localhost"
                && !host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            {
                return false;
            }
        }
    }
    if let Some(origin) = headers.get("origin") {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|value| reqwest::Url::parse(value).ok());
        match (origin, host_url.as_ref()) {
            (Some(origin), Some(host))
                if origin.username().is_empty()
                    && origin.password().is_none()
                    && origin.path() == "/"
                    && origin.query().is_none()
                    && origin.fragment().is_none()
                    && origin.origin() == host.origin() => {}
            _ => return false,
        }
    }
    !headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "cross-site")
}

async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> impl IntoResponse {
    if request
        .extensions()
        .get::<crate::local_ipc::LocalIpcRequest>()
        .is_some()
    {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if path == "/" || path == "/health" || path == "/dashboard" || path.starts_with("/dashboard/") {
        return next.run(request).await;
    }
    if !browser_request_allowed(
        &headers,
        state.token.is_some() || state.jwt_secret.is_some(),
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"browser origin or host is not allowed"})),
        )
            .into_response();
    }
    if state.tenant_id.is_some()
        && (path.starts_with("/provider-memory/mcp/") || path == "/provider-memory/captures")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"workspace MCP operations are unavailable in tenant mode"})),
        )
            .into_response();
    }
    if is_workspace_operation_path(path) {
        if let Some(expected) = state.token.as_deref() {
            let supplied = headers
                .get("authorization")
                .or_else(|| headers.get("x-api-key"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if token_is_valid(supplied, expected) {
                return next.run(request).await;
            }
        }
    }
    if let Some(secret) = state.jwt_secret.as_deref() {
        let supplied = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = supplied.strip_prefix("Bearer ").unwrap_or("");
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_exp = true;
        let claims = match decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        ) {
            Ok(data) if !data.claims.sub.trim().is_empty() => data.claims,
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"detail":"unauthorized"})),
                )
                    .into_response()
            }
        };
        // JWT currently establishes a user identity only. Until the server
        // defines and validates an administrative role, JWT identities must
        // not read workspace-wide operational telemetry.
        if is_workspace_operation_path(path) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"detail":"workspace operations require server-token authentication"})),
            )
                .into_response();
        }
        request.extensions_mut().insert(AuthContext {
            user_id: claims.sub,
        });
        return next.run(request).await;
    }
    if let Some(expected) = state.token.as_deref() {
        let supplied = headers
            .get("authorization")
            .or_else(|| headers.get("x-api-key"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !token_is_valid(supplied, expected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"detail":"unauthorized"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn is_workspace_operation_path(path: &str) -> bool {
    // Provider hooks access the entire workspace and do not implement
    // per-user attribution or retrieval. A user JWT is insufficient.
    is_workspace_telemetry_path(path)
        || extension_http_path_registered(path)
        || path.starts_with("/integrations/openclaw/hooks/")
        || path == "/provider-memory/mcp/tools/call"
        || path == "/provider-memory/mcp/documents/upsert"
        || path == "/provider-memory/captures"
}

fn extension_http_path_registered(path: &str) -> bool {
    use membrick_extension_api::HttpMethod;
    [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Patch,
        HttpMethod::Delete,
        HttpMethod::Head,
        HttpMethod::Options,
    ]
    .into_iter()
    .any(|method| {
        crate::extension_host::registry()
            .match_http_route(method, path)
            .is_some()
    })
}

fn is_workspace_telemetry_path(path: &str) -> bool {
    matches!(
        path,
        "/api/events"
            | "/api/captures"
            | "/api/integrations"
            | "/api/sessions"
            | "/api/status"
            | "/api/timeseries"
            | "/api/search-reviews"
            | "/api/extensions/events"
            | "/api/metrics"
            | "/metrics"
    ) || path.starts_with("/api/sessions/")
}

#[derive(Debug, serde::Deserialize)]
struct Claims {
    sub: String,
}

#[derive(Clone, Debug)]
struct AuthContext {
    user_id: String,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The server has no landing page of its own, so the root opens the dashboard.

/// Ask the tool's owning extension for an opaque dashboard change notice.
/// Clients refetch content through their authenticated read APIs.
fn change_event(tool: &str, payload: &Value) -> Option<Value> {
    crate::extension_host::registry().change_notification(tool, payload)
}

/// Server-sent opaque extension change notices for this workspace. The stream
/// opens with `ready`, then forwards each registered extension notification.
/// A client that falls behind receives `resync` and should refetch its view.

#[derive(Debug, Deserialize)]
struct DashboardSearchReviewsQuery {
    offset: Option<usize>,
    page_size: Option<usize>,
}

fn recent_provider_events(root: &std::path::Path) -> Vec<ProviderLifecycleEvent> {
    let mut events = dashboard_integrations(root)
        .into_iter()
        .flat_map(|integration| integration.events)
        .collect::<Vec<_>>();
    events.sort_by_key(|event| std::cmp::Reverse(event.timestamp_ms));
    events.truncate(100);
    events
}

fn dashboard_index_status(inspection: IndexStoreInspection) -> DashboardIndexStatus {
    let snapshot = inspection.snapshot.map(|snapshot| DashboardSnapshotStatus {
        layout: snapshot.layout,
        segment_count: snapshot.segment_count,
        global_document_count: snapshot.global_document_count,
        segments: snapshot
            .segments
            .into_iter()
            .map(|segment| DashboardSegmentStatus {
                segment_id: segment.segment_id,
                document_count: segment.document_count,
                profile_term_count: segment.profile_term_count,
                profile_entity_count: segment.profile_entity_count,
                profile_topic_count: segment.profile_topic_count,
                profile_local_memory_count: segment.profile_local_memory_count,
            })
            .collect(),
    });
    DashboardIndexStatus {
        source_document_count: inspection.source_document_count,
        record_count: inspection.record_count,
        dirty: inspection.dirty,
        store_revision: inspection.store_revision,
        snapshot_revision: inspection.snapshot_revision,
        revision_lag: inspection
            .store_revision
            .saturating_sub(inspection.snapshot_revision),
        snapshot,
    }
}

fn dashboard_integrations(root: &std::path::Path) -> Vec<DashboardIntegrationStatus> {
    vec![
        dashboard_integration("Claude Code", cfg!(feature = "claude-code"), "claude", root),
        dashboard_integration("Codex", cfg!(feature = "codex"), "codex", root),
        dashboard_integration(
            "Gemini CLI",
            cfg!(feature = "gemini-cli"),
            "gemini-cli",
            root,
        ),
        dashboard_integration("AGY", cfg!(feature = "agy"), "agy", root),
        dashboard_integration("OpenClaw", cfg!(feature = "openclaw"), "openclaw", root),
        dashboard_integration("Hermes", cfg!(feature = "hermes"), "hermes", root),
    ]
}

fn dashboard_integration(
    provider: &'static str,
    compiled: bool,
    recording_provider: &'static str,
    root: &std::path::Path,
) -> DashboardIntegrationStatus {
    let observed = provider_lifecycle_status(root, recording_provider)
        .ok()
        .flatten();
    let (state, note) = match observed.as_ref() {
        Some(status) if now_unix_ms().saturating_sub(status.last_seen_ms) <= 60_000 => (
            "active",
            "Lifecycle events have been received in the last minute.",
        ),
        Some(_) => (
            "idle",
            "Lifecycle events were received, but none arrived in the last minute.",
        ),
        None => (
            "not_observed",
            "No provider lifecycle events have been received yet.",
        ),
    };
    let status = observed.unwrap_or_default();
    let counters = DashboardProviderCounters::from_ledger(&status.counters);
    DashboardIntegrationStatus {
        event_window_limit: crate::telemetry::MAX_PROVIDER_TELEMETRY_EVENTS,
        counters,
        milestone_events: status.milestone_events,
        provider,
        recording_provider,
        compiled,
        state,
        note,
        last_event: status.last_event,
        last_seen_ms: (status.last_seen_ms > 0).then_some(status.last_seen_ms),
        events_total: status.events_total,
        sessions_started: status.sessions_started,
        sessions_ended: status.sessions_ended,
        sessions_active: status.sessions_active,
        retrieval_events: status.retrieval_events,
        capture_events: status.capture_events,
        indexes: dashboard_provider_indexes(root, recording_provider),
        events: status.events,
    }
}

fn discover_index_paths(root: &std::path::Path) -> Vec<PathBuf> {
    let directory = root.join(".membrick");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path.join("metadata.json").is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name == "workspace-memory"
                            || name.ends_with("-mcp-index")
                            || name.ends_with("-memory")
                    })
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn dashboard_provider_indexes(
    root: &std::path::Path,
    provider: &str,
) -> Vec<DashboardProviderIndex> {
    discover_index_paths(root)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_string();
            let prefix = name
                .strip_suffix("-mcp-index")
                .or_else(|| name.strip_suffix("-memory"))?;
            let normalized = if provider == "gemini-cli" {
                "gemini"
            } else {
                provider
            };
            if prefix != normalized {
                return None;
            }
            let inspection = MemoryService::at_path_without_owner_lock(
                &path,
                memory_pipeline_options(
                    None,
                    false,
                    false,
                    false,
                    true,
                    SegmentRoutingStrategy::TypedEvidenceMultiplicative,
                ),
            )
            .ok()?
            .inspection();
            let snapshot = inspection.snapshot.map(|snapshot| DashboardSnapshotStatus {
                layout: snapshot.layout,
                segment_count: snapshot.segment_count,
                global_document_count: snapshot.global_document_count,
                segments: snapshot
                    .segments
                    .into_iter()
                    .map(|segment| DashboardSegmentStatus {
                        segment_id: segment.segment_id,
                        document_count: segment.document_count,
                        profile_term_count: segment.profile_term_count,
                        profile_entity_count: segment.profile_entity_count,
                        profile_topic_count: segment.profile_topic_count,
                        profile_local_memory_count: segment.profile_local_memory_count,
                    })
                    .collect(),
            });
            Some(DashboardProviderIndex {
                name,
                role: if path.file_name()?.to_str()?.ends_with("-memory") {
                    "memory".to_string()
                } else {
                    "mcp".to_string()
                },
                source_document_count: inspection.source_document_count,
                record_count: inspection.record_count,
                dirty: inspection.dirty,
                store_revision: inspection.store_revision,
                snapshot_revision: inspection.snapshot_revision,
                snapshot,
            })
        })
        .collect()
}

fn tenant_check(
    state: &AppState,
    value: &Value,
    auth: Option<&AuthContext>,
) -> Result<(), (StatusCode, Json<Value>)> {
    if let Some(auth) = auth {
        if value.get("user_id").and_then(Value::as_str) != Some(auth.user_id.as_str()) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"detail":"user is not authorized"})),
            ));
        }
    }
    if let Some(expected) = state.tenant_id.as_deref() {
        if value.get("user_id").and_then(Value::as_str) != Some(expected) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"detail":"tenant is not authorized"})),
            ));
        }
    }
    Ok(())
}

#[derive(Default, serde::Deserialize)]
struct WriteOptions {
    #[serde(default)]
    wait_for_visibility: bool,
}

/// Provider hooks share `.membrick/memory` regardless of the server's primary
/// `--index`. Search composes that store with the workspace-memory index.
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
#[cfg(any(feature = "claude-code", feature = "codex"))]
#[cfg(any(feature = "claude-code", feature = "codex"))]

/// Journal provider documents through the resident owner's single upsert
/// path. The reply means the documents are durable; they become searchable
/// on the next background publication.

fn write_mutation<T: serde::Serialize + serde::de::DeserializeOwned + 'static>(
    state: &AppState,
    mutation: impl FnOnce(&mut MemoryService) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    state
        .staged_writer
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("resident memory writer is unavailable"))?
        .mutate_blocking(mutation)
}

fn write_workspace_mutation<T: serde::Serialize + serde::de::DeserializeOwned + 'static>(
    state: &AppState,
    mutation: impl FnOnce(&mut MemoryService) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    state
        .staged_writer
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("resident memory writer is unavailable"))?
        .for_workspace()
        .mutate_blocking(mutation)
}

fn ensure_bind_allowed(
    bind: &str,
    allow_non_loopback: bool,
    authentication_configured: bool,
    allow_unauthenticated: bool,
) -> anyhow::Result<()> {
    use std::net::ToSocketAddrs;
    let mut resolved = false;
    for address in bind.to_socket_addrs()? {
        resolved = true;
        if !address.ip().is_loopback() && !allow_non_loopback {
            anyhow::bail!(
                "refusing non-localhost bind address {}; pass --allow-non-loopback to opt in",
                address
            );
        }
    }
    anyhow::ensure!(resolved, "bind address resolved to no addresses: {bind}");
    if allow_non_loopback {
        anyhow::ensure!(
            authentication_configured && !allow_unauthenticated,
            "--allow-non-loopback requires token or JWT authentication and cannot be combined with --allow-unauthenticated"
        );
    }
    Ok(())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let mut difference = left.len() ^ right.len();
    for i in 0..left.len().max(right.len()) {
        difference |=
            usize::from(left.get(i).copied().unwrap_or(0) ^ right.get(i).copied().unwrap_or(0));
    }
    difference == 0
}

fn token_is_valid(supplied: &str, expected: &str) -> bool {
    constant_time_eq(supplied, expected)
        || constant_time_eq(supplied, &format!("Bearer {expected}"))
        || constant_time_eq(supplied, &format!("Token {expected}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIALIZATION_TEST_CAPABILITY: membrick_extension_api::CapabilityId =
        membrick_extension_api::CapabilityId::new("test-invocation-serialization");
    const SERIALIZATION_TEST_ENTRY_CAPABILITY: membrick_extension_api::CapabilityId =
        membrick_extension_api::CapabilityId::new("test-invocation-serialization.entry");
    const SERIALIZATION_TEST_TOOLS: &[membrick_extension_api::ToolDescriptor] = &[
        membrick_extension_api::ToolDescriptor {
            id: membrick_extension_api::ToolId::new("test_serialization_outer"),
            description: "Test-only blocking outer mutation.",
            input_schema: r#"{"type":"object"}"#,
        },
        membrick_extension_api::ToolDescriptor {
            id: membrick_extension_api::ToolId::new("test_serialization_second"),
            description: "Test-only competing mutation.",
            input_schema: r#"{"type":"object"}"#,
        },
    ];
    static SERIALIZATION_TEST_DESCRIPTOR: membrick_extension_api::ExtensionDescriptor =
        membrick_extension_api::ExtensionDescriptor {
            id: membrick_extension_api::ExtensionId::new("membrick-test-invocation-serialization"),
            provides: &[SERIALIZATION_TEST_ENTRY_CAPABILITY],
            requires: &[SERIALIZATION_TEST_CAPABILITY],
            tools: SERIALIZATION_TEST_TOOLS,
        };
    static SERIALIZATION_TEST_PROVIDER_DESCRIPTOR: membrick_extension_api::ExtensionDescriptor =
        membrick_extension_api::ExtensionDescriptor {
            id: membrick_extension_api::ExtensionId::new(
                "membrick-test-invocation-serialization-provider",
            ),
            provides: &[SERIALIZATION_TEST_CAPABILITY],
            requires: &[],
            tools: &[],
        };

    #[derive(Default)]
    struct InvocationGate {
        nested_entered: bool,
        release_nested: bool,
        second_entered: bool,
    }

    static INVOCATION_GATE: (std::sync::Mutex<InvocationGate>, std::sync::Condvar) = (
        std::sync::Mutex::new(InvocationGate {
            nested_entered: false,
            release_nested: false,
            second_entered: false,
        }),
        std::sync::Condvar::new(),
    );

    struct SerializationTestExtension;

    impl membrick_extension_api::ExtensionHandler for SerializationTestExtension {
        fn tool_is_mutating(&self, tool: membrick_extension_api::ToolId) -> bool {
            matches!(
                tool.as_str(),
                "test_serialization_outer" | "test_serialization_second"
            )
        }

        fn call(
            &self,
            context: &mut dyn membrick_extension_api::ExtensionContext,
            tool: membrick_extension_api::ToolId,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, membrick_extension_api::ExtensionError> {
            match tool.as_str() {
                "test_serialization_outer" => context.invoke(
                    SERIALIZATION_TEST_CAPABILITY,
                    "wait_inside_nested_capability",
                    serde_json::Value::Null,
                ),
                "test_serialization_second" => {
                    let (gate, changed) = &INVOCATION_GATE;
                    gate.lock()
                        .map_err(|_| {
                            membrick_extension_api::ExtensionError::new("test gate poisoned")
                        })?
                        .second_entered = true;
                    changed.notify_all();
                    Ok(serde_json::json!({"second": "entered"}))
                }
                _ => Err(membrick_extension_api::ExtensionError::new(
                    "unexpected test tool",
                )),
            }
        }

        fn invoke(
            &self,
            _context: &mut dyn membrick_extension_api::ExtensionContext,
            capability: membrick_extension_api::CapabilityId,
            operation: &str,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, membrick_extension_api::ExtensionError> {
            if capability != SERIALIZATION_TEST_CAPABILITY
                || operation != "wait_inside_nested_capability"
            {
                return Err(membrick_extension_api::ExtensionError::new(
                    "unexpected test capability call",
                ));
            }
            let (gate, changed) = &INVOCATION_GATE;
            let mut state = gate
                .lock()
                .map_err(|_| membrick_extension_api::ExtensionError::new("test gate poisoned"))?;
            state.nested_entered = true;
            changed.notify_all();
            while !state.release_nested {
                state = changed.wait(state).map_err(|_| {
                    membrick_extension_api::ExtensionError::new("test gate poisoned")
                })?;
            }
            Ok(serde_json::json!({"nested": "completed"}))
        }
    }

    static SERIALIZATION_TEST_HANDLER: SerializationTestExtension = SerializationTestExtension;

    #[membrick_extension_api::distributed_slice(membrick_extension_api::EXTENSIONS)]
    static SERIALIZATION_TEST_REGISTRATION: membrick_extension_api::ExtensionRegistration =
        membrick_extension_api::ExtensionRegistration {
            descriptor: &SERIALIZATION_TEST_DESCRIPTOR,
            handler: &SERIALIZATION_TEST_HANDLER,
        };
    #[membrick_extension_api::distributed_slice(membrick_extension_api::EXTENSIONS)]
    static SERIALIZATION_TEST_PROVIDER_REGISTRATION: membrick_extension_api::ExtensionRegistration =
        membrick_extension_api::ExtensionRegistration {
            descriptor: &SERIALIZATION_TEST_PROVIDER_DESCRIPTOR,
            handler: &SERIALIZATION_TEST_HANDLER,
        };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutating_extension_call_holds_serialization_through_nested_capability() {
        *INVOCATION_GATE.0.lock().unwrap() = InvocationGate::default();
        let registry = crate::extension_host::registry();
        assert!(registry
            .tool_is_mutating_name("test_serialization_outer")
            .unwrap());
        assert!(registry
            .tool_is_mutating_name("test_serialization_second")
            .unwrap());

        let owner = std::sync::Arc::new(std::sync::RwLock::new(MemoryService::in_memory(
            PipelineOptions {
                key_phrase_enrichment: false,
                ..Default::default()
            },
        )));
        let writer = StagedWriter::start(owner).unwrap();
        let first_writer = writer.clone();
        let first = tokio::spawn(async move {
            first_writer
                .mutate(move |service| {
                    crate::integrations::mcp_tools::dispatch_extension_tool(
                        "test_serialization_outer",
                        &serde_json::json!({}),
                        service,
                        "owner",
                        "/workspace",
                        "test-provider",
                    )
                    .map_err(anyhow::Error::msg)
                })
                .await
        });

        let entered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if INVOCATION_GATE.0.lock().unwrap().nested_entered {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();

        let second_writer = writer.clone();
        let second = tokio::spawn(async move {
            second_writer
                .mutate(move |service| {
                    crate::integrations::mcp_tools::dispatch_extension_tool(
                        "test_serialization_second",
                        &serde_json::json!({}),
                        service,
                        "owner",
                        "/workspace",
                        "test-provider",
                    )
                    .map_err(anyhow::Error::msg)
                })
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let entered_while_nested_call_blocked = INVOCATION_GATE.0.lock().unwrap().second_entered;
        {
            let (gate, changed) = &INVOCATION_GATE;
            gate.lock().unwrap().release_nested = true;
            changed.notify_all();
        }

        let (first_result, second_result) =
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                tokio::join!(first, second)
            })
            .await
            .expect("serialized extension mutations did not finish");
        assert!(
            entered,
            "outer extension call did not enter its nested capability"
        );
        assert_eq!(
            first_result.unwrap().unwrap(),
            serde_json::json!({"nested":"completed"})
        );
        assert_eq!(
            second_result.unwrap().unwrap(),
            serde_json::json!({"second":"entered"})
        );
        assert!(
            !entered_while_nested_call_blocked,
            "the competing mutation entered before the nested capability returned"
        );
    }

    #[test]
    fn server_settings_load_relative_freshness_and_default_to_relative() {
        let settings: ServerSettings =
            toml::from_str("[freshness]\nmode = 'relative'\nhalf_life_days = 45.0\nweight = 0.3\n")
                .expect("freshness settings should deserialize");
        let policy = settings.freshness.unwrap().validate().unwrap();
        assert_eq!(policy.mode, crate::temporal::FreshnessMode::Relative);
        assert_eq!(policy.half_life_days, 45.0);
        assert_eq!(policy.weight, 0.3);

        let defaults: ServerSettings = toml::from_str("").expect("empty settings are valid");
        assert_eq!(
            defaults.freshness.unwrap_or_default().mode,
            crate::temporal::FreshnessMode::Relative
        );
    }

    pub(super) fn security_test_root() -> PathBuf {
        static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "membrick-http-security-{}-{}-{}",
            std::process::id(),
            now_unix_ms(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    fn security_test_state(root: &std::path::Path) -> AppState {
        security_test_state_with_allowed_workspaces(root, std::iter::empty::<PathBuf>())
    }

    fn security_test_state_with_allowed_workspaces(
        root: &std::path::Path,
        additional_roots: impl IntoIterator<Item = PathBuf>,
    ) -> AppState {
        let owner = open_workspace_owner_at(
            root,
            root,
            crate::memory_api::workspace::segmented_store_options(),
        )
        .unwrap();
        let owner_lock = crate::memory_api::workspace::WorkspaceOwnerLock::acquire(root).unwrap();
        let service = Arc::new(RwLock::new(owner));
        let writer = StagedWriter::start(service.clone()).unwrap();
        let writer_gate = Arc::new(tokio::sync::Mutex::new(()));
        let schedule = server_writer::WriteSchedule {
            refresh_interval: Duration::from_millis(250),
            batch_size: 512,
            checkpoint_interval: Duration::from_secs(30),
        };
        let allowed_roots = std::iter::once(root.to_path_buf())
            .chain(additional_roots)
            .collect();
        let workspace_registry = Arc::new(WorkspaceRegistry::new(
            crate::memory_api::workspace::segmented_store_options(),
            schedule,
            allowed_roots,
            DEFAULT_MAX_ACTIVE_WORKSPACES,
        ));
        let owner_lock = Arc::new(owner_lock);
        workspace_registry.register_primary(WorkspaceRuntime {
            #[cfg(any(feature = "claude-code", feature = "codex"))]
            transcript_captures: start_transcript_captures(root, &writer, &owner_lock).unwrap(),
            service: service.clone(),
            staged_writer: writer.clone(),
            writer_gate: writer_gate.clone(),
            _owner_lock: owner_lock,
            project_root: root.to_path_buf(),
            workspace_index_root: root.to_path_buf(),
        });
        AppState {
            service,
            writer_gate,
            staged_writer: Some(writer),
            token: None,
            jwt_secret: Some("test-secret".into()),
            tenant_id: None,
            telemetry: OperationalTelemetry::new(),
            project_root: root.to_path_buf(),
            workspace_index_root: root.to_path_buf(),
            workspace_watcher: None,
            workspace_registry,
        }
    }

    fn provider_memory_test_state(root: &std::path::Path) -> AppState {
        provider_memory_test_state_with_allowed_workspaces(root, std::iter::empty::<PathBuf>())
    }

    fn provider_memory_test_state_with_allowed_workspaces(
        root: &std::path::Path,
        additional_roots: impl IntoIterator<Item = PathBuf>,
    ) -> AppState {
        let mut state = security_test_state_with_allowed_workspaces(root, additional_roots);
        state.jwt_secret = None;
        state
    }

    #[tokio::test]
    async fn browser_requests_cannot_rebind_or_cross_origins_to_local_memory() {
        use tower::ServiceExt;
        let root = security_test_root();
        let app = app_router(provider_memory_test_state(&root));
        for (host, origin) in [
            ("attacker.example:8080", "http://attacker.example:8080"),
            ("127.0.0.1:8080", "http://attacker.example"),
            ("127.0.0.1:8080", "null"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/status")
                        .header("host", host)
                        .header("origin", origin)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "browser request reached local memory: host={host}, origin={origin}"
            );
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .header("host", "127.0.0.1:8080")
                    .header("origin", "http://127.0.0.1:8080")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "codex")]
    #[tokio::test]
    async fn resident_capture_returns_before_indexing_and_drains_without_another_hook() {
        use tower::ServiceExt;
        let root = security_test_root();
        let transcript = root.join("capture.jsonl");
        let mut text = json!({"type":"response_item", "payload":{"type":"message", "role":"user", "id":"capture-user", "content":[{"type":"input_text","text":"remember the sapphire capture beacon"}]}}).to_string() + "\n";
        for _ in 0..5 {
            text.push_str(&json!({"type":"ignored", "padding":"x".repeat(600_000)}).to_string());
            text.push('\n');
        }
        text.push_str(&json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":"capture-answer", "content":[{"type":"output_text","text":"The sapphire capture beacon is ready."}]}}).to_string());
        text.push('\n');
        std::fs::write(&transcript, &text).unwrap();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        // A slow index must not prevent the hook from saving its request.
        let blocked_index = state.service.write().unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), app.oneshot(
            axum::http::Request::builder().method("POST")
                .uri("/provider-memory/captures")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({
                    "workspace":root, "provider":"codex", "document_type":"outcome",
                    "input":{"session_id":"capture-backlog", "hook_event_name":"Stop", "cwd":root, "transcript_path":transcript}
                }).to_string())).unwrap()
        )).await.expect("capture acknowledgement waited for the index").unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        drop(blocked_index);
        let state_dir = crate::integrations::session_recording::provider_state_dir(
            crate::integrations::session_recording::RecordingProvider::Codex,
            &root,
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if crate::integrations::transcript_cursor::load_offset(
                    &state_dir,
                    "capture-backlog",
                ) == text.len() as u64
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("backlog did not drain without a second hook");
        state.staged_writer.as_ref().unwrap().flush().await.unwrap();
        let view = state.provider_published_view().unwrap();
        assert!(view
            .source_documents()
            .iter()
            .any(|doc| doc.content.contains("sapphire capture beacon is ready")));
    }

    #[cfg(feature = "codex")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_queue_capture_continues_after_request_transport_stops() {
        let root = security_test_root();
        let transcript = root.join("queue-capture.jsonl");
        let mut text=json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Retain the violet capture lantern."}]}}).to_string()+"\n";
        for _ in 0..3 {
            text.push_str(
                &(json!({"type":"ignored", "padding":"x".repeat(600_000)}).to_string() + "\n"),
            );
        }
        text.push_str(&(json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"The violet capture lantern is ready."}]}}).to_string()+"\n"));
        std::fs::write(&transcript, &text).unwrap();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let pump_root = root.clone();
        let pump = tokio::spawn(async move {
            loop {
                crate::local_ipc::process_queue_once(&app, &pump_root)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let client_root = root.clone();
        let (status,body)=tokio::task::spawn_blocking(move || crate::local_ipc::queue_post_json(&client_root,"/provider-memory/captures",&json!({
            "workspace":client_root,"provider":"codex","document_type":"outcome",
            "input":{"session_id":"file-queue-capture","cwd":client_root,"hook_event_name":"Stop","transcript_path":transcript}
        }))).await.unwrap().unwrap();
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        pump.abort();
        let _ = pump.await;
        let runtime = state
            .workspace_registry
            .resolve(&root.to_string_lossy())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if runtime
                    .transcript_captures
                    .status()
                    .unwrap()
                    .iter()
                    .any(|s| s.session_id == "file-queue-capture" && s.status == "caught-up")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        runtime.staged_writer.flush().await.unwrap();
        let result = runtime
            .staged_writer
            .read_view()
            .unwrap()
            .search_agent_cached(
                "violet capture lantern",
                "codex",
                None,
                10,
                &BTreeMap::new(),
            )
            .unwrap();
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("violet capture lantern is ready")),
            "{result}"
        );
        assert_eq!(
            runtime.transcript_captures.status().unwrap()[0].captured_bytes,
            text.len() as u64
        );
        runtime.transcript_captures.shutdown();
    }

    #[tokio::test]
    async fn tenant_mode_rejects_workspace_wide_mcp_routes() {
        use tower::ServiceExt;
        let root = security_test_root();
        let mut state = provider_memory_test_state(&root);
        state.tenant_id = Some(Arc::from("alice"));
        let app = app_router(state);
        for route in [
            "/provider-memory/mcp/tools/call",
            "/provider-memory/mcp/documents/upsert",
            "/provider-memory/captures",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(route)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            json!({
                                "workspace":root, "provider":"codex", "user_id":"alice",
                                "tool":"info", "arguments":{}
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "tenant credentials must not permit workspace-wide MCP operations: {route}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    // Behavioural regression: an acknowledged board post must appear in the
    // same published snapshot used by provider searches.
    #[cfg(feature = "flowboard-tests")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn board_write_acknowledgement_publishes_search_snapshot() {
        use tower::ServiceExt;
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let mut post_id = String::new();
        for (tool, arguments) in [
            (
                "board_open",
                json!({"key":"default","title":"Session board","session_id":"board-visibility"}),
            ),
            (
                "board_post",
                json!({"content":"quartz compass published board evidence","request_id":"board-visibility-post","session_id":"board-visibility"}),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/provider-memory/mcp/tools/call")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            json!({
                                "workspace":root,"provider":"codex","user_id":"mcp",
                                "session_id":"board-visibility","tool":tool,"arguments":arguments,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            if tool == "board_post" {
                post_id = result["post_id"].as_str().unwrap().to_string();
            }
        }
        assert!(
            state
                .provider_published_view()
                .unwrap()
                .source_document_by_id(&post_id)
                .is_some(),
            "acknowledged board post is absent from the published search snapshot"
        );
        drop(app);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn call_provider_tool(
        app: &Router,
        root: &std::path::Path,
        tool: &str,
        arguments: Value,
    ) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "workspace":root,"provider":"codex","user_id":"mcp",
                            "tool":tool,"arguments":arguments,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn encode_path_segment(value: &str) -> String {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

    /// Reads `event:`/`data:` records from a server-sent event body.
    struct SseReader {
        body: axum::body::Body,
        buffer: String,
    }

    impl SseReader {
        async fn open(app: &Router, authorization: Option<&str>) -> axum::response::Response {
            use tower::ServiceExt;
            let mut request = axum::http::Request::builder().uri("/api/extensions/events");
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            app.clone()
                .oneshot(request.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap()
        }

        /// The next event, or `None` if nothing arrives within `wait`.
        async fn next(&mut self, wait: Duration) -> Option<(String, Value)> {
            use http_body_util::BodyExt;
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                if let Some(end) = self.buffer.find("\n\n") {
                    let record = self.buffer[..end].to_string();
                    self.buffer.drain(..end + 2);
                    let mut name = String::from("message");
                    let mut data = String::new();
                    for line in record.lines() {
                        if let Some(value) = line.strip_prefix("event:") {
                            name = value.trim().to_string();
                        } else if let Some(value) = line.strip_prefix("data:") {
                            data.push_str(value.trim());
                        }
                    }
                    if data.is_empty() {
                        continue; // keep-alive comment
                    }
                    return Some((name, serde_json::from_str(&data).unwrap()));
                }
                let frame = tokio::time::timeout_at(deadline, self.body.frame())
                    .await
                    .ok()??
                    .unwrap();
                if let Ok(bytes) = frame.into_data() {
                    self.buffer.push_str(std::str::from_utf8(&bytes).unwrap());
                }
            }
        }
    }

    // A dashboard subscriber gets exactly one notice per board write, and
    // only after the dashboard read API already returns the written post.
    #[cfg(feature = "flowboard-tests")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn board_change_is_published_once_after_the_write_is_readable() {
        use tower::ServiceExt;
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let response = SseReader::open(&app, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        let mut events = SseReader {
            body: response.into_body(),
            buffer: String::new(),
        };
        let wait = Duration::from_secs(5);
        assert_eq!(events.next(wait).await.unwrap().0, "ready");

        let (status, board) = call_provider_tool(
            &app,
            &root,
            "board_open",
            json!({"key":"live-updates","title":"Live updates"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{board}");
        let board_id = board["board_id"].as_str().unwrap().to_string();
        let (name, opened) = events.next(wait).await.unwrap();
        assert_eq!(name, "change");
        assert_eq!(opened["kind"], "board");
        assert_eq!(opened["tool"], "board_open");
        assert_eq!(opened["board_id"], board_id.as_str());

        let (status, post) = call_provider_tool(
            &app,
            &root,
            "board_post",
            json!({"board_id":board_id,"content":"first live post","request_id":"live-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{post}");
        let (name, posted) = events.next(wait).await.unwrap();
        assert_eq!(name, "change");
        assert_eq!(posted["tool"], "board_post");
        assert_eq!(posted["board_id"], board_id.as_str());
        assert_eq!(posted["sequence"], post["sequence"]);

        let detail = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/boards/{}", encode_path_segment(&board_id)))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(detail.status(), StatusCode::OK);
        let detail: Value = serde_json::from_slice(
            &axum::body::to_bytes(detail.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            detail["posts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["sequence"] == post["sequence"]),
            "notified post is not visible to the dashboard: {detail}"
        );
        assert!(
            events.next(Duration::from_millis(300)).await.is_none(),
            "one extension write produced more than one notice"
        );
        drop(events);
        drop(app);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extension_events_require_the_server_token() {
        let root = security_test_root();
        let mut state = provider_memory_test_state(&root);
        state.token = Some("events-secret".into());
        let app = app_router(state.clone());
        assert_eq!(
            SseReader::open(&app, None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            SseReader::open(&app, Some("Bearer wrong")).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            SseReader::open(&app, Some("Bearer events-secret"))
                .await
                .status(),
            StatusCode::OK
        );
        drop(app);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lagged_extension_event_subscriber_is_told_to_resync() {
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let mut events = SseReader {
            body: SseReader::open(&app, None).await.into_body(),
            buffer: String::new(),
        };
        let wait = Duration::from_secs(5);
        assert_eq!(events.next(wait).await.unwrap().0, "ready");
        let changes = state.staged_writer.as_ref().unwrap().changes();
        for n in 0..300 {
            changes.publish(json!({"notice_id": n, "payload": {"opaque": true}}));
        }
        let (name, data) = events.next(wait).await.unwrap();
        assert_eq!(name, "resync");
        assert!(data["skipped"].as_u64().unwrap() > 0, "{data}");
        drop(events);
        drop(app);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    // Provider MCP processes forward Flow tools to the resident; it must
    // dispatch them instead of rejecting them as unknown.
    #[cfg(feature = "flowboard-tests")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resident_dispatches_forwarded_flow_tools() {
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let (status, result) = call_provider_tool(&app, &root, "flow_list", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result, json!({"flows": []}));
        drop(app);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "flowboard-tests")]
    #[test]
    fn change_events_identify_what_changed() {
        assert_eq!(
            change_event("board_post", &json!({"board_id":"board:a","sequence":3})),
            Some(json!({"kind":"board","tool":"board_post","board_id":"board:a","sequence":3}))
        );
        assert_eq!(
            change_event("board_open", &json!({"board_id":"board:a","key":"a"})),
            Some(json!({"kind":"board","tool":"board_open","board_id":"board:a","sequence":null}))
        );
        assert_eq!(
            change_event(
                "flow_event_append",
                &json!({"run":{"flow_id":"flow:1","board_id":"board:a","revision":4},"events":[]})
            ),
            Some(json!({
                "kind":"flow","tool":"flow_event_append","flow_id":"flow:1",
                "board_id":"board:a","revision":4,
            }))
        );
        assert_eq!(change_event("board_post", &json!({})), None);
        assert_eq!(change_event("flow_create", &json!({"flows":[]})), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staged_http_add_flush_wait_and_user_isolation() {
        use tower::ServiceExt;
        let root = security_test_root();
        let mut state = provider_memory_test_state(&root);
        let writer = StagedWriter::start_with_schedule(
            state.service.clone(),
            server_writer::WriteSchedule {
                refresh_interval: Duration::from_secs(60),
                batch_size: 10000,
                checkpoint_interval: Duration::from_secs(60),
            },
        )
        .unwrap();
        state.staged_writer = Some(writer);
        let app = app_router(state.clone());
        async fn post(app: &Router, uri: &str, payload: Value) -> (StatusCode, Value) {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                },
            )
        }
        let payload = json!({"request_id":"http-staged","session_id":"session","user_id":"alice","messages":[{"role":"user","content":"quartz staged HTTP memory"}]});
        let (status, ack) = post(&app, "/add", payload.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ack["published"], false);
        let (_, before) = post(
            &app,
            "/search",
            json!({"query":"quartz","user_id":"alice","top_k":10}),
        )
        .await;
        assert!(before["data"].as_array().unwrap().is_empty());
        assert_eq!(
            post(&app, "/flush", json!({})).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, after) = post(
            &app,
            "/search",
            json!({"query":"quartz","user_id":"alice","top_k":10}),
        )
        .await;
        assert_eq!(after["data"].as_array().unwrap().len(), 1);
        let (_, foreign) = post(
            &app,
            "/search",
            json!({"query":"quartz","user_id":"bob","top_k":10}),
        )
        .await;
        assert!(foreign["data"].as_array().unwrap().is_empty());
        let (_, receipt) = post(&app, "/add?wait_for_visibility=true", payload).await;
        assert!(receipt["adjudication"].is_object());
        assert_eq!(
            state
                .service
                .read()
                .unwrap()
                .memory_store(MemoryStore::Captured)
                .inspection()
                .source_document_count,
            1
        );

        let one_shot = json!({
            "user_id":"alice",
            "messages":[{"role":"user","content":"one-shot identity test"}]
        });
        let (first_status, first) = post(&app, "/v1/memories", one_shot.clone()).await;
        let (second_status, second) = post(&app, "/v1/memories", one_shot).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert!(first["request_id"].as_str().is_some());
        assert!(first["session_id"].as_str().is_some());
        assert_ne!(first["request_id"], second["request_id"]);
        assert_ne!(first["session_id"], second["session_id"]);
        let explicit_session = json!({
            "request_id":"http-explicit-session",
            "user_id":"alice",
            "session_id":"app-session-42",
            "messages":[{"role":"user","content":"explicit session identity test"}]
        });
        let (explicit_session_status, explicit_session_response) =
            post(&app, "/v1/memories", explicit_session).await;
        assert_eq!(explicit_session_status, StatusCode::OK);
        assert_eq!(explicit_session_response["session_id"], "app-session-42");
        assert_eq!(
            post(
                &app,
                "/v1/memories",
                json!({"messages":[{"role":"user","content":"missing owner"}]}),
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let retry = json!({
            "request_id":"http-stable-retry",
            "user_id":"alice",
            "messages":[{"role":"user","content":"explicit retry identity test"}]
        });
        let (retry_status, retry_first) = post(&app, "/v1/memories", retry.clone()).await;
        let (retry_again_status, retry_again) = post(&app, "/v1/memories", retry.clone()).await;
        assert_eq!(retry_status, StatusCode::OK);
        assert_eq!(retry_again_status, StatusCode::OK);
        assert_eq!(retry_first["request_id"], "http-stable-retry");
        assert_eq!(retry_first["session_id"], retry_again["session_id"]);
        assert_eq!(retry_first["request_id"], retry_again["request_id"]);
        let changed_retry = json!({
            "request_id":"http-stable-retry",
            "user_id":"alice",
            "messages":[{"role":"user","content":"changed retry payload"}]
        });
        assert_eq!(
            post(&app, "/v1/memories", changed_retry).await.0,
            StatusCode::CONFLICT
        );

        let (batch_status, batch) = post(
            &app,
            "/add/batch",
            json!([
                {"user_id":"alice","messages":[{"role":"user","content":"batch without ids one"}]},
                {"user_id":"alice","messages":[{"role":"user","content":"batch without ids two"}]}
            ]),
        )
        .await;
        assert_eq!(batch_status, StatusCode::OK);
        assert_eq!(batch.as_array().unwrap().len(), 2);
        assert_ne!(batch[0]["request_id"], batch[1]["request_id"]);
        assert_ne!(batch[0]["session_id"], batch[1]["session_id"]);
        let stable_batch = json!([{
            "request_id":"http-batch-retry",
            "user_id":"alice",
            "messages":[{"role":"user","content":"batch retry identity test"}]
        }]);
        let (batch_retry_status, batch_retry) =
            post(&app, "/add/batch", stable_batch.clone()).await;
        let (batch_again_status, batch_again) = post(&app, "/add/batch", stable_batch).await;
        assert_eq!(batch_retry_status, StatusCode::OK);
        assert_eq!(batch_again_status, StatusCode::OK);
        assert_eq!(batch_retry[0]["request_id"], "http-batch-retry");
        assert_eq!(batch_retry[0]["session_id"], batch_again[0]["session_id"]);
        let changed_batch_retry = json!([{
            "request_id":"http-batch-retry",
            "user_id":"alice",
            "messages":[{"role":"user","content":"changed batch retry payload"}]
        }]);
        assert_eq!(
            post(&app, "/add/batch", changed_batch_retry).await.0,
            StatusCode::CONFLICT
        );
        drop(app);
        drop(state);
        // The worker's final checkpoint may still be completing, so avoid
        // deleting its root here; the OS temp directory is test-owned.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_api_memory_survives_workspace_refresh() {
        use tower::ServiceExt;
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let app = app_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/add?wait_for_visibility=true")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "request_id":"api-memory-survives-refresh",
                            "session_id":"api-memory-session",
                            "user_id":"alice",
                            "messages":[{"role":"user","content":"quartz keepsake memory"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let api_memory_exists = state
            .service
            .read()
            .unwrap()
            .memory_store(MemoryStore::Captured)
            .source_documents()
            .iter()
            .any(|document| document.content.contains("quartz keepsake memory"));
        assert!(
            api_memory_exists,
            "HTTP API memory must use the captured-memory service"
        );

        state
            .staged_writer
            .as_ref()
            .unwrap()
            .for_workspace()
            .replace_workspace_documents(Vec::new())
            .await
            .unwrap();

        let after_refresh = state
            .service
            .read()
            .unwrap()
            .memory_store(MemoryStore::Captured)
            .source_documents()
            .iter()
            .any(|document| document.content.contains("quartz keepsake memory"));
        assert!(after_refresh, "workspace refresh must retain API memory");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_http_wait_keeps_the_accepted_write() {
        use tower::ServiceExt;
        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let writer = state.staged_writer.as_ref().unwrap().clone();
        let first = AddRequest {
            request_id: "before-cancel".into(),
            user_id: "alice".into(),
            session_id: "cancel-session".into(),
            messages: vec![crate::memory_api::Message {
                role: "user".into(),
                timestamp: None,
                content: "quartz first queued memory".into(),
                expires_at_ms: None,
                supersedes_id: None,
            }],
        };
        writer.add(vec![first], false).await.unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        state
            .service
            .write()
            .unwrap()
            .memory_store_mut(MemoryStore::Captured)
            .begin_publication_paused(gate)
            .unwrap();
        let app = app_router(state.clone());
        let request = tokio::spawn(async move {
            app.oneshot(axum::http::Request::builder().method("POST").uri("/add?wait_for_visibility=true")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({"request_id":"cancel-test","session_id":"cancel-session","user_id":"alice","messages":[{"role":"user","content":"quartz cancellation regression"}]}).to_string())).unwrap()).await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if state
                    .service
                    .read()
                    .unwrap()
                    .memory_store(MemoryStore::Captured)
                    .pending_document_count()
                    == 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        request.abort();
        let _ = request.await;
        release.send(()).unwrap();
        writer.flush().await.unwrap();
        let results = writer
            .read_view()
            .unwrap()
            .search_cached(SearchRequest {
                query: "quartz cancellation regression".into(),
                user_id: "alice".into(),
                top_k: 10,
                options: None,
                session_id: None,
                scope: None,
                filters: None,
                reference_date: None,
            })
            .unwrap();
        assert!(results
            .data
            .iter()
            .any(|hit| hit.content.contains("cancellation regression")));
        // Reopening verifies the cancelled request's accepted write was durable.
        let persisted = MemoryService::at_path_without_owner_lock(
            &crate::memory_api::workspace::shared_memory_root(&root),
            crate::memory_api::workspace::segmented_store_options(),
        )
        .unwrap();
        assert!(persisted
            .source_documents()
            .iter()
            .any(|doc| doc.content.contains("cancellation regression")));
    }

    fn security_test_token(claims: Value) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    #[cfg(feature = "openclaw")]
    #[tokio::test]
    async fn openclaw_http_hook_uses_the_matching_workspace_runtime() {
        use tower::ServiceExt;
        let root = security_test_root();
        let other = security_test_root();
        let app = app_router(provider_memory_test_state_with_allowed_workspaces(
            &root,
            std::iter::once(other.clone()),
        ));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/integrations/openclaw/hooks/agent-end")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "event":{
                                "runId":"multi-workspace-openclaw-run",
                                "messages":[
                                    {"role":"user","content":"Remember the turquoise lighthouse evacuation plan."},
                                    {"role":"assistant","content":"The evacuation plan uses the north stairwell."}
                                ]
                            },
                            "ctx":{"workspaceDir":other,"sessionId":"outside"}
                        })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response_body["ok"], true);
        assert!(other
            .join(".membrick")
            .join(crate::memory_api::workspace::WORKSPACE_MEMORY_NAME)
            .exists());
        assert!(other
            .join(".membrick")
            .join(crate::integrations::mcp_index::SHARED_MEMORY_DIR)
            .exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[cfg(feature = "openclaw")]
    #[tokio::test]
    async fn openclaw_http_hook_rejects_jwt_workspace_access() {
        use tower::ServiceExt;
        let root = security_test_root();
        let app = app_router(security_test_state(&root));
        let token = security_test_token(json!({"sub":"alice", "exp":4_102_444_800u64}));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/integrations/openclaw/hooks/bootstrap")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(
                        json!({"event":{"context":{"workspaceDir":root}},"query":"private memory"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn jwt_is_identity_only_and_cannot_read_workspace_telemetry() {
        use tower::ServiceExt;
        let root = security_test_root();
        let app = app_router(security_test_state(&root));
        let token = security_test_token(json!({"sub":"alice", "exp":4_102_444_800u64}));
        // A valid JWT can still use user-scoped memory APIs.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/v1/memories?user_id=alice")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // JWT has no role model; an undocumented `admin` claim grants nothing.
        for claims in [
            json!({"sub":"alice", "exp":4_102_444_800u64}),
            json!({"sub":"alice", "admin":true, "exp":4_102_444_800u64}),
        ] {
            let token = security_test_token(claims);
            for path in [
                "/api/events",
                "/api/sessions/victim/events",
                "/api/integrations",
                "/api/sessions",
                "/api/status",
                "/api/timeseries",
                "/api/metrics",
                "/metrics",
                "/provider-memory/mcp/tools/call",
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .header("authorization", format!("Bearer {token}"))
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "JWT telemetry exposed at {path}"
                );
            }
        }
        // The privilege claim is accepted only after signature validation.
        let forged = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &json!({"sub":"operator", "admin":true, "exp":4_102_444_800u64}),
            &jsonwebtoken::EncodingKey::from_secret(b"wrong-secret"),
        )
        .unwrap();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/events")
                    .header("authorization", format!("Bearer {forged}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn server_token_can_read_telemetry_when_jwt_auth_is_configured() {
        use tower::ServiceExt;
        let root = security_test_root();
        let mut state = security_test_state(&root);
        state.token = Some("operator-token".into());
        let app = app_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/events")
                    .header("authorization", "Bearer operator-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn jwt_search_scope_is_bound_to_subject() {
        use tower::ServiceExt;
        let root = security_test_root();
        let app = app_router(security_test_state(&root));
        let token = security_test_token(json!({"sub":"alice", "exp":4_102_444_800u64}));
        let body = json!({
            "query":"some unusual query for session state",
            "user_id":"alice",
            "session_id":"shared-session",
            "scope":"victim-scope",
            "top_k":5
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/search")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut states = crate::conversation_state::ConversationStateStore::open_under(
            &root.join(".membrick").join("memory"),
        );
        assert!(
            states
                .get("victim-scope", "shared-session", now_unix_ms())
                .is_none(),
            "request-provided scope contaminated another user's conversation state"
        );
        assert!(states
            .get("alice", "shared-session", now_unix_ms())
            .is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tenant_provider_search_does_not_expose_other_users() {
        use tower::ServiceExt;
        let root = security_test_root();
        let mut state = provider_memory_test_state(&root);
        state.staged_writer.as_ref().unwrap().add(vec![serde_json::from_value(json!({
            "request_id":"victim-note", "user_id":"bob", "session_id":"bob-session",
            "messages":[{"role":"assistant","content":"quartz telescope secret calibration procedure"}]
        })).unwrap()], true).await.unwrap();
        state.tenant_id = Some(Arc::from("alice"));
        let response = app_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/search")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"query":"quartz telescope secret calibration",
                    "user_id":"alice", "top_k":5})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["data"].as_array().unwrap().is_empty(),
            "tenant search exposed another user's memory: {body}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_memory_batch_writes_to_shared_store_and_search_sees_it() {
        use tower::ServiceExt;

        let root = security_test_root();
        let app = app_router(provider_memory_test_state(&root));
        let add = json!([{
            "request_id":"hermes:test-turn",
            "user_id":"hermes",
            "session_id":"hermes:test-session",
            "messages":[{"role":"assistant","content":"Shared provider memory stores the cobalt telescope calibration procedure."}]
        }]);
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/add/batch")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(add.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let shared_root = crate::integrations::mcp_index::shared_memory_root(&root);
        assert!(
            shared_root.exists(),
            "provider writes must use .membrick/memory"
        );

        // Use another integration's identity: server-token provider recall is
        // intentionally shared across provider-owned memories.
        let query = json!({
            "query":"cobalt telescope calibration",
            "user_id":"openclaw",
            "top_k":5
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/search")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(query.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let hits = body["data"].as_array().unwrap();
        assert!(hits.iter().any(|hit| hit["content"]
            .as_str()
            .unwrap_or_default()
            .contains("cobalt telescope")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resident_mcp_tool_route_publishes_add_memory_for_search() {
        use tower::ServiceExt;

        let root = security_test_root();
        let second_root = security_test_root();
        let state = provider_memory_test_state_with_allowed_workspaces(
            &root,
            std::iter::once(second_root.clone()),
        );
        let app = app_router(state.clone());
        let context = |tool: &str, arguments: Value| {
            json!({
                "workspace": root,
                "provider": "codex",
                "user_id": "mcp",
                "session_id": "codex:resident-test-session",
                "tool": tool,
                "arguments": arguments,
            })
        };
        let add = context(
            "add_memory",
            json!({
                "request_id":"codex:resident-test-write",
                "session_id":"codex:resident-test-session",
                "content":"The resident MCP backend keeps the cobalt telescope calibration procedure.",
            }),
        );
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(add.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let runtime = state
            .workspace_registry
            .resolve(&root.to_string_lossy())
            .await
            .unwrap();
        runtime
            .staged_writer
            .read_view()
            .unwrap()
            .note_active_session("codex", "codex:resident-test-session");
        let mut search = context(
            "search",
            json!({
                "query":"cobalt telescope calibration",
                "top_k":5,
            }),
        );
        search.as_object_mut().unwrap().remove("session_id");
        // The search inherits the active session established by add_memory.
        // Its session attribution is internal transport metadata, removed by
        // the provider adapter before the response reaches the agent.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(search.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["__membrick_telemetry"]["session_id"],
            "codex:resident-test-session"
        );
        assert_eq!(
            body["__membrick_telemetry"]["session_attribution"],
            "inferred"
        );
        assert!(body["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));

        // A second Codex process can use the same backend while keeping its
        // resident owner and captured memory isolated to its own project.
        let mut second_add = context(
            "add_memory",
            json!({
                "request_id":"codex:second-workspace-write",
                "session_id":"codex:second-workspace-session",
                "content":"The amber submarine uses a distinct pressure valve procedure.",
            }),
        );
        second_add["workspace"] = json!(second_root);
        second_add["session_id"] = json!("codex:second-workspace-session");
        second_add["arguments"]["session_id"] = json!("codex:second-workspace-session");
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(second_add.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut second_search = context(
            "search",
            json!({
                "query":"amber submarine pressure valve",
                "top_k":5,
                "session_id":"codex:second-workspace-session",
            }),
        );
        second_search["workspace"] = json!(second_root);
        second_search["session_id"] = json!("codex:second-workspace-session");
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(second_search.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(body["results"].as_array().is_some_and(|results| {
            results.iter().any(|hit| {
                hit["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("amber submarine")
            })
        }));

        // The same query against the first project must not see the second
        // project's provider memory.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        context(
                            "search",
                            json!({
                                "query":"amber submarine pressure valve",
                                "top_k":5,
                                "session_id":"codex:resident-test-session",
                            }),
                        )
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(body["results"].as_array().is_some_and(Vec::is_empty));

        let wrong_workspace = context("info", json!({}));
        let mut wrong_workspace = wrong_workspace;
        wrong_workspace["workspace"] = json!(root.join("missing"));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(wrong_workspace.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(second_root);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resident_hook_document_route_publishes_upsert_for_search() {
        use tower::ServiceExt;

        let root = security_test_root();
        let app = app_router(provider_memory_test_state(&root));
        let session_id = "codex:resident-hook-session";
        let content = "Codex session-summary: the quartz compass startup sequence".to_string();
        let document = crate::SourceDocument {
            doc_id: "resident-hook-document".to_string(),
            source: "codex://resident-hook/session-summary".to_string(),
            doc_length: content.len(),
            content: content.into(),
            concept: "session-summary".to_string(),
            group_id: Some("codex-session:resident-hook".to_string()),
            headings: vec!["session-summary".to_string()],
            links: vec![],
            timestamp: None,
            author_agent: Some("codex".to_string()),
            filters: std::collections::BTreeMap::from([
                ("provider".to_string(), "codex".to_string()),
                ("session_id".to_string(), session_id.to_string()),
                (
                    "tool_capture_id".to_string(),
                    "resident-capture".to_string(),
                ),
            ]),
            key_phrases: vec![],
            key_phrase_extraction_hash: String::new(),
        };
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/documents/upsert")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "workspace":root,
                            "provider":"codex",
                            "session_id":session_id,
                            "user_id":"mcp",
                            "document":document.clone(),
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let audit_path =
            crate::integrations::mcp_index::shared_memory_root(&root).join("mutations.jsonl");
        let audit = std::fs::read_to_string(&audit_path).unwrap();
        let audit_records = audit
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(audit_records.len(), 1);
        assert_eq!(audit_records[0]["action"], "capture:codex");
        assert_eq!(audit_records[0]["memory_ids"][0], "resident-hook-document");

        // The upsert reply means durable, not yet searchable; flush publishes.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/flush")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let search = json!({
            "workspace":root,
            "provider":"codex",
            "user_id":"mcp",
            "session_id":session_id,
            "tool":"search",
            "arguments":{"query":"quartz compass startup sequence","session_id":session_id},
        });
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(search.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body["results"].as_array().is_some_and(|results| {
            results.iter().any(|result| {
                result["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("quartz compass"))
            })
        }));

        let replacement_content = "Codex session-summary: the amber compass startup sequence";
        let mut replacement = document;
        replacement.doc_id = "resident-hook-document-replacement".to_string();
        replacement.source = "codex://resident-hook/session-summary/replacement".to_string();
        replacement.content = replacement_content.into();
        replacement.doc_length = replacement.content.len();
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/documents/upsert")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "workspace":root,
                            "provider":"codex",
                            "session_id":session_id,
                            "user_id":"mcp",
                            "replace_group":{"key":"tool_capture_id","value":"resident-capture"},
                            "documents":[replacement],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let audit = std::fs::read_to_string(&audit_path).unwrap();
        let audit_records = audit
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(audit_records.len(), 2);
        assert_eq!(
            audit_records[1]["memory_ids"][0],
            "resident-hook-document-replacement"
        );
        let replacement_search = json!({
            "workspace":root,
            "provider":"codex",
            "user_id":"mcp",
            "session_id":session_id,
            "tool":"search",
            "arguments":{"query":"amber compass startup sequence","session_id":session_id},
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(replacement_search.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body["results"].as_array().is_some_and(|results| {
            results.iter().any(|result| {
                result["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("amber compass"))
            })
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_and_provider_writers_publish_one_resident_view() {
        let root = security_test_root();
        let owner = open_workspace_owner(&root).unwrap();
        let owner = Arc::new(RwLock::new(owner));
        let schedule = server_writer::WriteSchedule {
            refresh_interval: Duration::from_millis(10),
            batch_size: 1,
            checkpoint_interval: Duration::from_secs(60),
        };
        let provider_writer = StagedWriter::start_with_schedule(owner.clone(), schedule).unwrap();
        let workspace_writer = provider_writer.for_workspace();
        let content = "The resident workspace view publishes edits without provider reopens";
        let mut document = crate::SourceDocument::with_stable_doc_id_from_source(
            "docs/resident-workspace-update.md".to_string(),
            content.to_string(),
            "resident workspace update".to_string(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        document.doc_id = document.source.clone();
        workspace_writer
            .replace_workspace_documents(vec![document.clone()])
            .await
            .unwrap();
        let published = provider_writer.read_view().unwrap();
        assert!(published
            .source_document_by_id(&document.doc_id)
            .is_some_and(|stored| stored.content == content));

        let mut replacement = crate::SourceDocument::with_stable_doc_id_from_source(
            "docs/resident-workspace-replacement.md".to_string(),
            "The replacement file is searchable after watcher publication".to_string(),
            "resident workspace replacement".to_string(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        replacement.doc_id = replacement.source.clone();
        workspace_writer
            .replace_workspace_documents(vec![replacement.clone()])
            .await
            .unwrap();
        let published = provider_writer.read_view().unwrap();
        assert!(published.source_document_by_id(&document.doc_id).is_none());
        assert!(published
            .source_document_by_id(&replacement.doc_id)
            .is_some_and(|stored| stored.content.contains("replacement file")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn codex_resident_client_uses_workspace_backend_for_add_and_search() {
        let root = security_test_root().canonicalize().unwrap();
        crate::state_paths::record_install_settings(&root).unwrap();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            crate::local_ipc::serve(
                app_router(provider_memory_test_state(&server_root)),
                &server_root,
                std::future::pending::<Option<std::fs::File>>(),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        });

        // Generous, because the server builds its test store before it
        // publishes the endpoint, and the full suite loads the machine.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if crate::local_ipc::socket_path(&root).is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resident IPC endpoint should be published before the client call");

        let session_id = "codex:resident-client-session";
        let add_root = root.clone();
        let add_arguments = json!({
            "request_id":"codex:resident-client-write",
            "session_id":session_id,
            "content":"The resident client stores the indigo telescope alignment sequence.",
        });
        let add = tokio::task::spawn_blocking(move || {
            crate::integrations::mcp_tools::call_resident_tool(
                Some(json!(1)),
                &add_root,
                "codex",
                "add_memory",
                &add_arguments,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(add.error.is_none(), "resident add failed: {add:?}");

        let search_root = root.clone();
        let search_arguments =
            json!({"query":"indigo telescope alignment", "session_id":session_id});
        let search = tokio::task::spawn_blocking(move || {
            crate::integrations::mcp_tools::call_resident_tool(
                Some(json!(2)),
                &search_root,
                "codex",
                "search",
                &search_arguments,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(search.error.is_none(), "resident search failed: {search:?}");
        let text = search.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let payload: Value = serde_json::from_str(&text).unwrap();
        assert!(payload["results"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));

        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn provider_memory_search_includes_workspace_documents() {
        use tower::ServiceExt;

        let root = security_test_root();
        std::fs::write(
            root.join("README.md"),
            "The vermilion observatory uses a brass meridian alignment checklist.",
        )
        .unwrap();
        let app = app_router(provider_memory_test_state(&root));
        let query = json!({
            "query":"vermilion observatory meridian alignment checklist",
            "user_id":"hermes",
            "top_k":5
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/search")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(query.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let hits = body["data"].as_array().unwrap();
        assert!(hits.iter().any(|hit| hit["content"]
            .as_str()
            .unwrap_or_default()
            .contains("vermilion observatory")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn jwt_provider_memory_search_remains_user_isolated() {
        use tower::ServiceExt;

        let root = security_test_root();
        MemoryService::with_shared_memory(&root, |service| {
            service.add_batch(vec![AddRequest {
                request_id: "bob-private-memory".to_string(),
                user_id: "bob".to_string(),
                session_id: "bob-session".to_string(),
                messages: vec![crate::memory_api::Message {
                    role: "assistant".to_string(),
                    timestamp: None,
                    content: "Bob's private obsidian sundial calibration details.".to_string(),
                    expires_at_ms: None,
                    supersedes_id: None,
                }],
            }])?;
            Ok(())
        })
        .unwrap();

        let app = app_router(security_test_state(&root));
        let token = security_test_token(json!({"sub":"alice", "exp":4_102_444_800u64}));
        let query = json!({
            "query":"obsidian sundial calibration",
            "user_id":"alice",
            "top_k":5
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/provider-memory/search")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(query.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body["data"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn server_uses_segmented_memory_index() {
        assert!(matches!(
            memory_pipeline_options(
                None,
                false,
                false,
                false,
                true,
                SegmentRoutingStrategy::TypedEvidenceMultiplicative
            )
            .memory_index_layout,
            MemoryIndexLayout::Segmented { .. }
        ));
    }

    #[test]
    fn server_adaptive_mode_is_opt_in() {
        assert!(matches!(
            memory_pipeline_options(
                Some(8),
                false,
                false,
                false,
                true,
                SegmentRoutingStrategy::TypedEvidenceMultiplicative
            )
            .memory_index_layout,
            MemoryIndexLayout::AdaptiveSegmented {
                query_top_n: 5,
                max_query_n: 8,
                ..
            }
        ));
    }

    #[test]
    fn server_adaptive_limit_at_or_below_base_keeps_fixed_mode() {
        for limit in [0, 2, 3, 4, 5] {
            assert!(matches!(
                memory_pipeline_options(
                    Some(limit),
                    false,
                    false,
                    false,
                    true,
                    SegmentRoutingStrategy::TypedEvidenceMultiplicative
                )
                .memory_index_layout,
                MemoryIndexLayout::Segmented { .. }
            ));
        }
    }

    #[test]
    fn server_defaults_to_coverage_local_routing() {
        let args = Args::try_parse_from(["membrick-server"]).unwrap();
        assert_eq!(args.segment_routing, SegmentRoutingArg::CoverageLocal);
        assert_eq!(
            SegmentRoutingArg::CoverageLocal.strategy(),
            SegmentRoutingStrategy::CoverageLocalDistinctiveness
        );
        let options = memory_pipeline_options(
            None,
            false,
            false,
            false,
            true,
            args.segment_routing.strategy(),
        );
        assert!(matches!(
            options.memory_index_layout,
            MemoryIndexLayout::Segmented {
                routing_strategy: SegmentRoutingStrategy::CoverageLocalDistinctiveness,
                ..
            }
        ));
    }

    #[test]
    fn segment_routing_flag_selects_each_strategy() {
        for (arg, expected) in [
            (
                SegmentRoutingArg::Sparse,
                SegmentRoutingStrategy::SparseOverlap,
            ),
            (
                SegmentRoutingArg::LocalDistinctiveness,
                SegmentRoutingStrategy::LocalDistinctiveness,
            ),
            (
                SegmentRoutingArg::CoverageLocal,
                SegmentRoutingStrategy::CoverageLocalDistinctiveness,
            ),
            (
                SegmentRoutingArg::CoverageTeam,
                SegmentRoutingStrategy::CoverageTeamSelection,
            ),
            (
                SegmentRoutingArg::TeamCoverageLocal,
                SegmentRoutingStrategy::TeamCoverageLocalDistinctiveness,
            ),
            (
                SegmentRoutingArg::TypedEvidenceAdditive,
                SegmentRoutingStrategy::TypedEvidence,
            ),
            (
                SegmentRoutingArg::GatedCoverageLocal,
                SegmentRoutingStrategy::TypedEvidenceMultiplicative,
            ),
            (
                SegmentRoutingArg::GatedCoverageTeam,
                SegmentRoutingStrategy::CoverageTeamTypedMultiplicative,
            ),
        ] {
            assert_eq!(arg.strategy(), expected);
            let options = memory_pipeline_options(None, false, false, false, true, arg.strategy());
            assert!(matches!(
                options.memory_index_layout,
                MemoryIndexLayout::Segmented {
                    routing_strategy,
                    ..
                } if routing_strategy == expected
            ));
        }
    }

    #[test]
    fn empty_jwt_secret_is_not_considered_configured() {
        assert!(normalize_secret(Some("   ".to_string())).is_none());
    }

    #[test]
    fn search_reads_see_writes_through_the_single_service_lock() {
        let mut service = MemoryService::in_memory(memory_pipeline_options(
            None,
            false,
            false,
            false,
            true,
            SegmentRoutingStrategy::TypedEvidenceMultiplicative,
        ));
        service
            .add(AddRequest {
                request_id: "r1".into(),
                messages: vec![crate::memory_api::Message {
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
        let state = AppState {
            service: Arc::new(RwLock::new(service)),
            writer_gate: Arc::new(tokio::sync::Mutex::new(())),
            staged_writer: None,
            token: None,
            jwt_secret: None,
            tenant_id: None,
            telemetry: OperationalTelemetry::new(),
            project_root: std::env::current_dir().unwrap(),
            workspace_index_root: std::env::current_dir()
                .unwrap()
                .join(".membrick")
                .join(crate::memory_api::workspace::WORKSPACE_MEMORY_NAME),
            workspace_watcher: None,
            workspace_registry: Arc::new(WorkspaceRegistry::new(
                crate::memory_api::workspace::segmented_store_options(),
                server_writer::WriteSchedule {
                    refresh_interval: Duration::from_millis(250),
                    batch_size: 512,
                    checkpoint_interval: Duration::from_secs(30),
                },
                HashSet::new(),
                DEFAULT_MAX_ACTIVE_WORKSPACES,
            )),
        };
        // Reads take the shared lock and see the write with no re-publish step.
        let response = state
            .service
            .write()
            .unwrap()
            .search(SearchRequest {
                query: "codename zephyr".into(),
                options: None,
                user_id: "user-a".into(),
                top_k: 10,
                session_id: None,
                scope: None,
                filters: None,
                reference_date: None,
            })
            .unwrap();
        assert!(response.data.iter().any(|m| m.content.contains("zephyr")));
    }

    #[test]
    fn token_accepts_supported_forms_and_rejects_wrong_values() {
        assert!(token_is_valid("secret", "secret"));
        assert!(token_is_valid("Bearer secret", "secret"));
        assert!(token_is_valid("Token secret", "secret"));
        assert!(!token_is_valid("Bearer other", "secret"));
        assert!(!token_is_valid("", &"\0".repeat(256)));
    }

    #[test]
    fn non_loopback_bind_requires_opt_in_and_authentication() {
        assert!(ensure_bind_allowed("127.0.0.1:8080", false, false, false).is_ok());
        assert!(ensure_bind_allowed("[::1]:8080", false, false, false).is_ok());
        assert!(ensure_bind_allowed("0.0.0.0:8080", false, true, false).is_err());
        assert!(ensure_bind_allowed("192.168.1.10:8080", true, false, false).is_err());
        assert!(ensure_bind_allowed("0.0.0.0:8080", true, true, false).is_ok());
        assert!(ensure_bind_allowed("0.0.0.0:8080", true, true, true).is_err());
        let blank_token_is_configured = normalize_secret(Some("   ".to_string())).is_some();
        assert!(
            ensure_bind_allowed("0.0.0.0:8080", true, blank_token_is_configured, false).is_err()
        );
    }

    #[tokio::test]
    async fn writer_gate_rejects_a_second_mutation() {
        let gate = tokio::sync::Mutex::new(());
        let _held = gate.lock().await;
        assert!(gate.try_lock().is_err());
    }

    #[cfg(feature = "openclaw")]
    #[tokio::test]
    async fn provider_memory_and_openclaw_http_writers_share_the_same_gate() {
        use tower::ServiceExt;

        let root = security_test_root();
        let state = provider_memory_test_state(&root);
        let gate = state.writer_gate.clone();
        let app = app_router(state);
        let _held = gate.lock().await;

        let writes = [
            (
                "/provider-memory/add/batch",
                json!([{
                    "request_id": "hermes-race-test",
                    "user_id": "hermes",
                    "session_id": "race-test",
                    "messages": [{"role": "user", "content": "race test"}]
                }]),
            ),
            (
                "/integrations/openclaw/hooks/agent-end",
                json!({"event": {"runId": "openclaw-race-test"}, "ctx": {}}),
            ),
        ];

        for (uri, payload) in writes {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{uri} must be rejected while the shared writer gate is held"
            );
        }
    }

    #[test]
    fn jwt_subject_can_be_decoded_with_expiry_validation() {
        let key = b"test-secret";
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({"sub":"user-a","exp":4_102_444_800u64}),
            &jsonwebtoken::EncodingKey::from_secret(key),
        )
        .unwrap();
        let claims = jsonwebtoken::decode::<Claims>(
            &token,
            &jsonwebtoken::DecodingKey::from_secret(key),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .unwrap()
        .claims;
        assert_eq!(claims.sub, "user-a");
    }
}
