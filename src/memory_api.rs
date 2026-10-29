//! Memory Add/Search API backed by Membrick's `IndexStore`.

use crate::conversational_rerank::{conversational_rerank, RERANK_DEEP_TOP_K, RERANK_WEIGHTS};
use crate::index::ClaimStatus;
use crate::pipeline::PipelineOptions;
#[cfg(test)]
use crate::pipeline::Tier1NerProvider;
use crate::query_plan::PreparedQuery;
use crate::query_semantics::{QueryEvidenceOrder, QueryEvidenceRelation};
use crate::segments::relations::{
    analyze_fact_question, extract_relations_via_spacy, query_structured, relation_turns_from_docs,
    try_extract_key_phrases_via_spacy, RelationIndex,
};
use crate::session_prepare::is_follow_up;
use crate::{IndexStore, SourceDocument};
use chrono::{NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
pub(crate) mod workspace;

#[cfg(any(feature = "claude-code", feature = "codex"))]
pub(crate) mod transcript_capture;

mod enrich;
mod key_phrases;
mod mem0_filter;
mod receipts;
mod review;
mod search;
mod staged;
#[cfg(test)]
mod tests;

use key_phrases::KeyPhraseEnrichment;
pub use mem0_filter::{Mem0FilterExpr, Mem0FilterOperator};
use review::{SearchHistoryIndex, SearchReviewStateIndex};
use search::RelationsCache;
pub use staged::StagedAddResponse;

const USER_FILTER: &str = "memory_user_id";
const SESSION_OBSERVATION_DOCUMENT_TYPE: &str = "session-observation";

/// A durable request ID was reused with a payload that differs from its
/// original request. Callers must treat this as a definitive conflict.
#[derive(Debug)]
pub(crate) struct RequestFingerprintConflict;

impl std::fmt::Display for RequestFingerprintConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request_id was already used with different content")
    }
}

impl std::error::Error for RequestFingerprintConflict {}

fn is_session_observation(document: &SourceDocument) -> bool {
    document
        .filters
        .get("document_type")
        .is_some_and(|kind| kind == SESSION_OBSERVATION_DOCUMENT_TYPE)
}

fn is_workspace_markdown_document(document: &SourceDocument) -> bool {
    document.source == document.doc_id
        && Path::new(&document.source)
            .extension()
            .is_some_and(|extension| extension == "md")
}

fn workspace_documents_equal(left: &SourceDocument, right: &SourceDocument) -> bool {
    left.doc_id == right.doc_id
        && left.source == right.source
        && left.content == right.content
        && left.concept == right.concept
        && left.group_id == right.group_id
        && left.headings == right.headings
        && left.links == right.links
        && left.timestamp == right.timestamp
        && left.doc_length == right.doc_length
        && left.author_agent == right.author_agent
        && left.filters == right.filters
        && left.key_phrase_extraction_hash == right.key_phrase_extraction_hash
        && left.key_phrases.len() == right.key_phrases.len()
        && left
            .key_phrases
            .iter()
            .zip(&right.key_phrases)
            .all(|(left, right)| left.text == right.text && left.kind == right.kind)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRequest {
    pub request_id: String,
    pub messages: Vec<Message>,
    pub user_id: String,
    pub session_id: String,
}

/// Add request extension that carries user metadata without changing the
/// source-compatible `AddRequest` struct-literal contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRequestWithMetadata {
    #[serde(flatten)]
    pub request: AddRequest,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl AddRequestWithMetadata {
    /// Build a metadata-bearing add request. Direct service calls treat the
    /// supplied session ID as explicit.
    pub fn new(request: AddRequest, metadata: BTreeMap<String, serde_json::Value>) -> Self {
        Self { request, metadata }
    }
}

/// Internal provenance that cannot be supplied through the public service DTO.
#[derive(Debug, Clone)]
pub(crate) struct AddRequestContext {
    pub(crate) request: AddRequestWithMetadata,
    pub(crate) session_id_was_omitted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub timestamp: Option<i64>,
    pub content: String,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    #[serde(default)]
    pub supersedes_id: Option<String>,
}

/// Response to a successful `add`.
///
/// Compatibility note: the `adjudication` field was added after the initial
/// release. It is `#[serde(default)]`, so JSON produced by older versions
/// (without the field) still deserializes. Rust callers constructing
/// `AddResponse` with a struct literal must add the field; prefer
/// `..Default::default()`-style construction if available, or update
/// literals when upgrading.
#[derive(Debug, Serialize, Deserialize)]
pub struct AddResponse {
    pub success: bool,
    pub request_id: String,
    pub user_id: String,
    pub session_id: String,
    /// Write-adjudication receipt: what the semantic layer extracted from
    /// this write and what it decided. `None` only on deserialization of
    /// responses produced before receipts existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjudication: Option<WriteAdjudicationReceipt>,
}

/// A single claim extracted from a written document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjudicatedClaim {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub scope: String,
    pub evidence: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion_status: Option<crate::index::ClaimStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_evidence: Option<crate::index::ClaimEvidence>,
}

/// One clash decision involving a written document: what the semantic layer
/// decided about this write relative to one other document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjudicationDecision {
    /// "supersedes" | "conflicts_with" | "confirms"
    pub kind: String,
    pub other_doc_id: String,
    /// True when this write's document is the newer/correcting side of the
    /// relation (relation source); false when it is the older side.
    pub is_source: bool,
    pub confidence: f32,
    pub method: String,
    pub evidence: Vec<String>,
}

/// Write-adjudication receipt: the semantic layer's account of what a single
/// `add` write did. Lets callers audit supersession from the outside instead
/// of inferring it from later query results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteAdjudicationReceipt {
    pub request_id: String,
    pub doc_ids: Vec<String>,
    /// Claims extracted from this write's documents.
    pub claims: Vec<AdjudicatedClaim>,
    /// Clash decisions involving this write's documents.
    pub decisions: Vec<AdjudicationDecision>,
    /// Pre-existing documents this write actually retired (their semantic
    /// state is Superseded with superseded_by pointing at this write's
    /// document). Documents from the same request are never listed here.
    pub retired_doc_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[allow(dead_code)]
    pub options: Option<Vec<String>>,
    pub user_id: String,
    pub top_k: usize,
    /// Optional conversation session. When supplied, the search loads the
    /// bounded prior state for (user_id, session_id) and resolves follow-up
    /// phrasing and temporal anchors against it before retrieval. Absent or
    /// empty means stateless search: the query is analyzed on its own. This is
    /// a state key only; it never becomes an implicit `group_id` filter.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Conversation-state scope. When supplied, follow-up resolution and
    /// the conversational reranker scope to this value instead of `user_id`.
    /// This lets callers like the benchmark share conversation state across
    /// a fixed scope while filtering documents by a different user.
    #[serde(default)]
    pub scope: Option<String>,
    /// Additional document filters (e.g. `provider`) merged with the
    /// user-ownership filter. Absent means no additional filtering.
    #[serde(default)]
    pub filters: Option<BTreeMap<String, String>>,
    /// Reference date for resolving relative temporal language ("two weeks
    /// ago", "last Tuesday"). When supplied, relative dates resolve against
    /// this instead of the machine clock. Format: "2023/05/05" or similar.
    /// Absent means use system time (production default).
    #[serde(default)]
    pub reference_date: Option<String>,
}

/// Additive wire DTO for typed Mem0-style filters without changing the
/// source-compatible `SearchRequest` fields.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequestWithFilterAst {
    #[serde(flatten)]
    pub request: SearchRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_ast: Option<Mem0FilterExpr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub data: Vec<SearchMemory>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteRequest {
    pub user_id: String,
    #[serde(default)]
    pub doc_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SupersedeRequest {
    pub user_id: String,
    pub replacement_id: String,
    pub old_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRequest {
    pub user_id: String,
    #[serde(default)]
    pub memory_id: String,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRequest {
    pub user_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse {
    pub data: Vec<MemoryRecord>,
    pub next_cursor: Option<String>,
    /// Number of records matching ownership, visibility, session, and filter
    /// criteria before cursor pagination is applied.
    #[serde(default)]
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub user_id: String,
    #[serde(default)]
    pub memory_id: String,
    pub content: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

/// Update request extension that preserves the existing `UpdateRequest` API.
/// `None` leaves metadata unchanged; `Some` replaces user metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequestWithMetadata {
    #[serde(flatten)]
    pub request: UpdateRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    /// Keep the stored expiry when the request does not supply one.
    #[serde(default)]
    pub preserve_expiration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    pub role: Option<String>,
    pub user_id: String,
    pub session_id: Option<String>,
    pub created_at: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub supersedes_id: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMemory {
    pub id: String,
    pub content: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub role: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_id: Option<String>,
    /// Why a structured-fact hit was returned (e.g. "shared relation: Rome").
    /// Empty for lexical hits; skipped in serialization when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relation_evidence: Vec<String>,
    /// Comparison sides for which this document was independently retrieved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparison_operands: Vec<String>,
    /// Claim level lifecycle and assertion state for the returned source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_states: Vec<crate::semantic_relations::SemanticClaimState>,
}

/// Backing corpus selected by the resident workspace owner. Callers enqueue
/// mutations; only MemoryService chooses and exposes the internal service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryStore {
    Captured,
    Workspace,
}

impl MemoryStore {
    pub(crate) const fn slot(self) -> usize {
        match self {
            Self::Captured => 0,
            Self::Workspace => 1,
        }
    }
}

pub struct MemoryService {
    store: IndexStore,
    /// Keeps standalone service instances from overlapping a project's
    /// resident owner. Resident-owned services use the internal constructor
    /// while their WorkspaceRuntime holds this same lock.
    workspace_owner_lock: Option<crate::state_paths::ProjectOwnerLock>,
    /// Private workspace backing service owned by the resident memory owner.
    /// The one writer selects either this service or the shared captured-memory
    /// `store`; publication composes their snapshots for resident searches.
    workspace_store: Option<Box<MemoryService>>,
    /// Shared immutable search snapshot published by the resident writer.
    /// The owner and HTTP writer retain the same handle so searches never
    /// need to reopen or recompose either backing store.
    published_search_view: Option<Arc<std::sync::RwLock<Arc<MemoryService>>>>,
    superseded_ids: HashSet<(String, String)>,
    has_expiring_documents: bool,
    /// Transcript observations are retained as source evidence, but are
    /// excluded from all MemoryService retrieval candidates.
    has_session_observations: bool,
    request_fingerprints: HashMap<(String, String), String>,
    /// Cached write-adjudication receipts by (user_id, request_id), so an
    /// idempotent retry returns the identical response including the receipt.
    request_receipts: HashMap<(String, String), WriteAdjudicationReceipt>,
    /// Where receipts persist across restarts. None for in-memory services.
    receipts_path: Option<std::path::PathBuf>,
    /// Receipt writes that must succeed before their cached values can be evicted.
    pending_receipt_persistence: HashSet<(String, String)>,
    pending_adds: HashMap<(String, String), Vec<String>>,
    publication: Option<staged::PendingPublication>,
    conversation_states:
        std::sync::Arc<std::sync::Mutex<crate::conversation_state::ConversationStateStore>>,
    /// Lazily-built dependency-parse relation index for the structured-fact
    /// path. The write path never pays for it; see [`RelationsCache`].
    relations_cache: Mutex<RelationsCache>,
    /// Background key-phrase enrichment (see [`KeyPhraseEnrichment`]).
    enrichment: std::sync::Arc<Mutex<KeyPhraseEnrichment>>,
    /// Immutable workspace-specific query rules, replaced atomically by the
    /// project rules watcher after a complete overlay validates.
    project_rules: Option<Arc<crate::reasoning_rules::ProjectReasoningRules>>,
    /// Resident-local locator map reconstructed from the authoritative MCP
    /// telemetry file. Read views share it; no query or result bodies live here.
    search_history_index: Arc<SearchHistoryIndex>,
    /// Resident-local feedback map reconstructed once from its durable file.
    /// Pagination checks this map instead of rereading the full JSON file.
    search_review_state: Arc<Mutex<SearchReviewStateIndex>>,
}

#[derive(Debug, Serialize)]
struct MutationAuditRecord<'a> {
    recorded_at_ms: u64,
    action: &'a str,
    user_id: &'a str,
    request_id: Option<&'a str>,
    memory_ids: &'a [String],
}

fn acquire_owner_lock_for_store(
    index_root: &Path,
) -> anyhow::Result<Option<crate::state_paths::ProjectOwnerLock>> {
    let absolute = if index_root.is_absolute() {
        index_root.to_path_buf()
    } else {
        std::env::current_dir()?.join(index_root)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    let mut existing = normalized.clone();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            anyhow::bail!("cannot resolve store path {}", index_root.display());
        };
        missing.push(name.to_os_string());
        if !existing.pop() {
            anyhow::bail!("cannot resolve store path {}", index_root.display());
        }
    }
    let existing = existing.canonicalize()?;
    let mut resolved_store = existing.clone();
    for component in missing.iter().rev() {
        resolved_store.push(component);
    }
    let Some(project_root) = crate::state_paths::find_project_root(&existing)? else {
        return Ok(None);
    };
    if !resolved_store.starts_with(project_root.join(".membrick")) {
        return Ok(None);
    }
    crate::state_paths::ProjectOwnerLock::acquire(&project_root)
        .map(Some)
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot open project store {} while its resident owns the workspace; connect through the project resident client instead: {error:#}",
                index_root.display(),
            )
        })
}

impl MemoryService {
    /// Record the provider session associated with the next implicit MCP
    /// search. Hook adapters call this service entry point rather than writing
    /// the conversation-state sidecar directly. The caller decides whether a
    /// memory store already exists, so lifecycle hooks do not create one just
    /// to record a session pointer.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn note_active_session_at_path(
        index_root: impl AsRef<Path>,
        provider: &str,
        session_id: &str,
    ) {
        crate::conversation_state::ConversationStateStore::open_under(index_root.as_ref())
            .note_active_session(provider, session_id);
    }

    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn deactivate_session_at_path(
        index_root: impl AsRef<Path>,
        provider: &str,
        session_id: &str,
    ) {
        crate::conversation_state::ConversationStateStore::open_under(index_root.as_ref())
            .deactivate_session(provider, session_id);
    }
}

impl MemoryService {
    /// Create an in-memory service without exposing the storage implementation
    /// to application callers.
    pub fn in_memory(options: PipelineOptions) -> Self {
        Self::new(IndexStore::in_memory(options))
    }

    /// Adapt a prebuilt CLI query snapshot to the service search path. The
    /// snapshot and its records are reused, so routing a CLI query through
    /// `search_with_filters` does not repeat corpus extraction or indexing.
    pub(crate) fn from_query_index(
        index: crate::index::MemoryIndex,
        options: PipelineOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self::new(IndexStore::from_query_index(index, options)?))
    }

    /// The CLI uses the service-owned snapshot to format its query results
    /// into the existing aggregate and LLM-context output shapes.
    pub(crate) fn query_index(&self) -> Option<&crate::index::MemoryIndex> {
        match self.store.memory_index_snapshot()? {
            crate::pipeline::MemoryIndexSnapshot::Single(index) => Some(index),
            crate::pipeline::MemoryIndexSnapshot::Segmented(_) => None,
        }
    }

    /// Expose raw index-stage evidence to the single-question harness. This
    /// uses the same resident snapshot as normal service searches.
    pub(crate) fn query_index_with_diagnostics(
        &self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &BTreeMap<String, String>,
    ) -> anyhow::Result<(
        Vec<crate::SearchResult>,
        crate::index::QueryTimings,
        crate::index::QueryDiagnostics,
    )> {
        self.store
            .query_prepared_cached_with_diagnostics(prepared, top_k, filters)
    }

    /// Open a persistent memory service without exposing `IndexStore` in the
    /// application-facing construction API. For an installed project's
    /// `.membrick` store, this acquires the exclusive project-owner lock and
    /// retains it for the service lifetime. It fails while the resident owns
    /// the project; use `Memory::new(project_root)` to connect to that resident.
    pub fn at_path(index_root: impl AsRef<Path>, options: PipelineOptions) -> anyhow::Result<Self> {
        let index_root = index_root.as_ref();
        let workspace_owner_lock = acquire_owner_lock_for_store(index_root)?;
        Self::open_at_path(index_root, options, workspace_owner_lock)
    }

    /// Open a store already protected by the resident's workspace owner lock.
    /// Keep this crate-private: callers outside the resident must use
    /// [`MemoryService::at_path`], which acquires and retains the lock itself.
    pub(crate) fn at_path_without_owner_lock(
        index_root: impl AsRef<Path>,
        options: PipelineOptions,
    ) -> anyhow::Result<Self> {
        Self::open_at_path(index_root.as_ref(), options, None)
    }

    fn open_at_path(
        index_root: &Path,
        options: PipelineOptions,
        workspace_owner_lock: Option<crate::state_paths::ProjectOwnerLock>,
    ) -> anyhow::Result<Self> {
        let mut store = IndexStore::at_path(index_root, options)?;
        let legacy_observations = store
            .source_documents()
            .into_iter()
            .filter(|document| is_session_observation(document))
            .map(|document| document.doc_id.clone())
            .collect::<Vec<_>>();
        if !legacy_observations.is_empty() {
            store.remove_many(legacy_observations.iter().map(String::as_str));
            store.refresh()?;
            tracing::info!(
                "removed {} legacy session-observation documents from the searchable index",
                legacy_observations.len()
            );
        }
        let mut service = Self::new(store);
        service.workspace_owner_lock = workspace_owner_lock;
        service.conversation_states = std::sync::Arc::new(std::sync::Mutex::new(
            crate::conversation_state::ConversationStateStore::open_under(index_root),
        ));
        // Receipts persist alongside the index so idempotent retries after a
        // restart return the identical response, including the adjudication.
        service.receipts_path = Some(index_root.join("receipts.json"));
        service.load_receipts();
        service.recover_staged_adds()?;
        Ok(service)
    }

    /// Appends only newly created receipts. Rewriting the full receipt cache
    /// made every write O(total historical requests); the journal keeps the
    /// persistent write cost proportional to this mutation's batch size.
    /// Failure is logged but never fails the write because retries can rebuild
    /// receipts from the persisted source documents.
    fn persist_receipts(&mut self, keys: &[(String, String)]) {
        let Some(path) = self.receipts_path.clone() else {
            return;
        };
        let keys = keys
            .iter()
            .chain(self.pending_receipt_persistence.iter())
            .cloned()
            .collect::<HashSet<_>>();
        self.pending_receipt_persistence
            .extend(keys.iter().cloned());
        let entries: Vec<((String, String), WriteAdjudicationReceipt)> = keys
            .iter()
            .filter_map(|key| {
                self.request_receipts
                    .get(key)
                    .map(|receipt| (key.clone(), receipt.clone()))
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        match serde_json::to_vec(&entries) {
            Ok(mut json) => {
                json.push(b'\n');
                let journal = path.with_extension("jsonl");
                let result = (|| -> std::io::Result<()> {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(journal)?;
                    file.write_all(&json)
                })();
                if let Err(e) = result {
                    tracing::warn!("failed toappend adjudication receipts: {e}");
                } else {
                    self.pending_receipt_persistence.clear();
                    self.trim_receipt_cache();
                }
            }
            Err(e) => tracing::warn!("failed toserialize adjudication receipts: {e}"),
        }
    }

    /// Append a content-free durable event for a memory API mutation. This
    /// journal records identifiers and operation metadata, never memory text.
    fn append_mutation_audit(
        &self,
        action: &str,
        user_id: &str,
        request_id: Option<&str>,
        memory_ids: &[String],
    ) -> anyhow::Result<()> {
        let Some(receipts_path) = self.receipts_path.as_ref() else {
            return Ok(());
        };
        let record = MutationAuditRecord {
            recorded_at_ms: unix_time_ms(),
            action,
            user_id,
            request_id,
            memory_ids,
        };
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let path = receipts_path.with_file_name("mutations.jsonl");
            let mut line = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
            line.push(b'\n');
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path)?;
            file.write_all(&line)?;
            file.sync_all()
        })();
        result.map_err(|error| {
            anyhow::anyhow!("failed to append durable memory mutation audit: {error}")
        })
    }

    /// Record a provider-hook capture after its documents have been published.
    /// Kept on `MemoryService` so integrations never write the journal directly.
    pub(crate) fn record_capture_mutation(
        &self,
        provider: &str,
        user_id: &str,
        session_id: &str,
        memory_ids: &[String],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !memory_ids.is_empty(),
            "capture audit requires document IDs"
        );
        let action = format!("capture:{provider}");
        self.append_mutation_audit(&action, user_id, Some(session_id), memory_ids)?;
        Ok(())
    }

    pub(crate) fn new(store: IndexStore) -> Self {
        crate::extension_host::initialize()
            .expect("compiled-in extensions must initialize successfully");
        // Eagerly load the WordNet/ConceptNet lexical expansion store at
        // service creation, not on the first query. The store is a
        // process-wide OnceLock; lazy init costs ~2s (decompress + parse
        // 2.3MB gzipped JSON) on the first query's hot path.
        crate::preload_lexical_store();
        let source_documents = store.source_documents();
        let has_expiring_documents = source_documents
            .iter()
            .any(|doc| doc.filters.contains_key("expires_at_ms"));
        let has_session_observations = source_documents
            .iter()
            .any(|doc| is_session_observation(doc));
        let superseded_ids = source_documents
            .iter()
            .into_iter()
            .filter_map(|doc| {
                Some((
                    doc.filters.get(USER_FILTER)?.clone(),
                    doc.filters.get("supersedes_id")?.clone(),
                ))
            })
            .collect();
        let request_fingerprints = source_documents
            .into_iter()
            .filter_map(|doc| {
                let persisted = doc.filters.get("request_fingerprint")?;
                let fingerprint = if persisted.starts_with("v2:") || persisted.starts_with("v3:") {
                    persisted.clone()
                } else {
                    let messages = serde_json::from_str::<Vec<Message>>(persisted).ok()?;
                    request_fingerprint(doc.group_id.as_deref()?, &messages, &BTreeMap::new())
                        .ok()?
                };
                Some((
                    (
                        doc.filters.get(USER_FILTER)?.clone(),
                        doc.filters.get("request_id")?.clone(),
                    ),
                    fingerprint,
                ))
            })
            .collect();
        Self {
            store,
            workspace_owner_lock: None,
            workspace_store: None,
            published_search_view: None,
            superseded_ids,
            has_expiring_documents,
            has_session_observations,
            request_fingerprints,
            request_receipts: HashMap::new(),
            receipts_path: None,
            pending_receipt_persistence: HashSet::new(),
            pending_adds: HashMap::new(),
            publication: None,
            conversation_states: std::sync::Arc::new(std::sync::Mutex::new(
                crate::conversation_state::ConversationStateStore::new(None),
            )),
            relations_cache: Mutex::new(RelationsCache::default()),
            enrichment: std::sync::Arc::new(Mutex::new(KeyPhraseEnrichment::default())),
            project_rules: None,
            search_history_index: Arc::new(SearchHistoryIndex::default()),
            search_review_state: Arc::new(Mutex::new(SearchReviewStateIndex::default())),
        }
    }

    /// Return bounded structural information for operational status views.
    /// Callers that expose this information should sanitize identifiers and
    /// persistence paths before sending it outside the process.
    pub fn inspection(&self) -> crate::pipeline::IndexStoreInspection {
        self.store.inspection()
    }

    /// CLI-only diagnostics: serialize records without exposing storage handles.
    pub(crate) fn diagnostic_records(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(self.store.records())?)
    }

    /// Publish pending mutations. Normal mutation methods already call this;
    /// this method is provided for hosts that batch lower-level changes.
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        self.drain_enrichment_inbox();
        self.flush()
    }

    pub fn add(&mut self, request: AddRequest) -> anyhow::Result<AddResponse> {
        self.add_with_metadata(AddRequestWithMetadata {
            request,
            metadata: BTreeMap::new(),
        })
    }

    pub fn add_with_metadata(
        &mut self,
        request: AddRequestWithMetadata,
    ) -> anyhow::Result<AddResponse> {
        self.add_with_metadata_context(AddRequestContext {
            request,
            session_id_was_omitted: false,
        })
    }

    pub(crate) fn add_with_metadata_context(
        &mut self,
        context: AddRequestContext,
    ) -> anyhow::Result<AddResponse> {
        self.drain_enrichment_inbox();
        let request_key = (
            context.request.request.user_id.clone(),
            context.request.request.request_id.clone(),
        );
        let (mut response, doc_ids) = self.add_unpublished_with_context(context)?;
        self.flush()?;
        self.attach_receipt(&mut response, request_key, &doc_ids);
        if !doc_ids.is_empty() {
            self.persist_receipts(&[(response.user_id.clone(), response.request_id.clone())]);
            self.append_mutation_audit(
                "add",
                &response.user_id,
                Some(&response.request_id),
                &doc_ids,
            )?;
        }
        Ok(response)
    }

    /// Adds several requests and publishes one snapshot after all mutations.
    /// Each request retains the normal per-request message limit and validation.
    /// Each response carries its own write-adjudication receipt, built after
    /// the single refresh so cross-request relations within the batch are
    /// attributed correctly.
    pub fn add_batch(&mut self, requests: Vec<AddRequest>) -> anyhow::Result<Vec<AddResponse>> {
        if requests.is_empty() {
            anyhow::bail!("requests must not be empty");
        }
        self.drain_enrichment_inbox();
        let mut responses = Vec::with_capacity(requests.len());
        let mut receipt_inputs = Vec::with_capacity(requests.len());
        for request in requests {
            let request_key = (request.user_id.clone(), request.request_id.clone());
            let (response, doc_ids) = self.add_unpublished(request)?;
            receipt_inputs.push((request_key, doc_ids));
            responses.push(response);
        }
        self.flush()?;
        let new_receipt_keys: Vec<_> = receipt_inputs
            .iter()
            .filter(|(_, doc_ids)| !doc_ids.is_empty())
            .map(|(request_key, _)| request_key.clone())
            .collect();
        let audit_inputs = receipt_inputs.clone();
        for (response, (request_key, doc_ids)) in
            responses.iter_mut().zip(receipt_inputs.into_iter())
        {
            self.attach_receipt(response, request_key, &doc_ids);
        }
        self.persist_receipts(&new_receipt_keys);
        for (response, (_, doc_ids)) in responses.iter().zip(audit_inputs.iter()) {
            if !doc_ids.is_empty() {
                self.append_mutation_audit(
                    "add",
                    &response.user_id,
                    Some(&response.request_id),
                    doc_ids,
                )?;
            }
        }
        Ok(responses)
    }

    /// Attaches the adjudication receipt to a response after refresh.
    /// Fresh writes build, cache, and persist it. Retries resolve from the
    /// cache, which is populated at open from disk, by earlier writes in
    /// this lifetime, or by earlier requests in the same batch — so a
    /// duplicate request inside one batch gets the identical receipt.
    fn attach_receipt(
        &mut self,
        response: &mut AddResponse,
        request_key: (String, String),
        doc_ids: &[String],
    ) {
        if !doc_ids.is_empty() {
            let receipt = self.build_adjudication_receipt(response.request_id.clone(), doc_ids);
            self.request_receipts.insert(request_key, receipt.clone());
            response.adjudication = Some(receipt);
        } else {
            response.adjudication =
                self.request_receipts
                    .get(&request_key)
                    .cloned()
                    .or_else(|| {
                        let receipt = self.rebuild_receipt(&request_key)?;
                        self.request_receipts
                            .insert(request_key.clone(), receipt.clone());
                        Some(receipt)
                    });
            self.trim_receipt_cache();
        }
    }

    /// Returns the semantic state (Current / Superseded / Conflicted) of one
    /// document. Used by the false-positive audit and other external
    /// verification of supersession decisions.
    pub fn semantic_document_state(
        &self,
        doc_id: &str,
    ) -> crate::semantic_relations::DocumentSemanticState {
        self.store.semantic_document_state(doc_id)
    }

    /// Claim states preserve independently current and superseded facts within
    /// a compound source note.
    pub fn semantic_claim_states(
        &self,
        doc_id: &str,
    ) -> Vec<crate::semantic_relations::SemanticClaimState> {
        self.store.semantic_claim_states(doc_id)
    }

    /// Return temporal facts whose validity interval includes `date`.
    /// `recorded_at` on each fact records when Membrick learned it, while
    /// `valid_from` and `valid_to` describe when it was true in the world.
    pub fn temporal_facts_as_of(&self, date: &str) -> Vec<crate::TemporalFact> {
        self.store
            .temporal_facts_as_of(date)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Return the facts Membrick would have considered true on `valid_date`,
    /// using only evidence recorded by `recorded_at`. The latter accepts an
    /// RFC 3339 timestamp or a date in `YYYY-MM-DD` form.
    pub fn temporal_facts_as_known_at(
        &self,
        valid_date: &str,
        recorded_at: &str,
    ) -> Vec<crate::TemporalFact> {
        self.store
            .temporal_facts_as_known_at(valid_date, recorded_at)
    }

    /// Return the temporal history for a subject, ordered by valid time.
    pub fn temporal_timeline(&self, subject: &str) -> Vec<crate::TemporalFact> {
        self.store
            .temporal_timeline(subject)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Return facts in the half-open temporal range `[start, end)`.
    pub fn temporal_events_between(
        &self,
        start: &str,
        end: &str,
    ) -> Vec<(crate::TemporalFact, chrono::NaiveDate)> {
        self.store
            .temporal_events_between(start, end)
            .into_iter()
            .map(|event| (event.fact.clone(), event.date))
            .collect()
    }

    /// Return temporal facts around an anchor date, with their resolved dates.
    pub fn temporal_timeline_window_around(
        &self,
        anchor: &str,
        before: usize,
        after: usize,
    ) -> Vec<(crate::TemporalFact, chrono::NaiveDate)> {
        self.store
            .temporal_timeline_window_around(anchor, before, after)
            .into_iter()
            .map(|event| (event.fact.clone(), event.date))
            .collect()
    }

    /// Return adjacent fact pairs in `[start, end)`, optionally limited by
    /// the maximum gap in days.
    pub fn temporal_adjacent_pairs_between(
        &self,
        start: &str,
        end: &str,
        max_gap_days: Option<i64>,
    ) -> Vec<(
        (crate::TemporalFact, chrono::NaiveDate),
        (crate::TemporalFact, chrono::NaiveDate),
        i64,
    )> {
        self.store
            .temporal_adjacent_pairs_between(start, end, max_gap_days)
            .into_iter()
            .map(|pair| {
                (
                    (pair.first.fact.clone(), pair.first.date),
                    (pair.second.fact.clone(), pair.second.date),
                    pair.gap_days,
                )
            })
            .collect()
    }

    /// Rebuilds a write-adjudication receipt for a request whose documents
    /// were persisted by an earlier process lifetime. Doc IDs are stable
    /// (user_id + request_id + message index) and each doc carries the
    /// request_id in its filters, so the original write's documents can be
    /// located without the in-memory cache. Returns None when no documents
    /// for the request exist.
    fn rebuild_receipt(&self, request_key: &(String, String)) -> Option<WriteAdjudicationReceipt> {
        if let Some(receipt) = self.persisted_receipt(request_key) {
            return Some(receipt);
        }
        let (user_id, request_id) = request_key;
        let doc_ids: Vec<String> = self
            .store
            .source_documents()
            .into_iter()
            .filter(|doc| {
                doc.filters.get("request_id").map(|s| s.as_str()) == Some(request_id.as_str())
                    && doc.filters.get(USER_FILTER).map(|s| s.as_str()) == Some(user_id.as_str())
            })
            .map(|doc| doc.doc_id.clone())
            .collect();
        if doc_ids.is_empty() {
            return None;
        }
        Some(self.build_adjudication_receipt(request_id.clone(), &doc_ids))
    }

    /// Builds the write-adjudication receipt for one request from the
    /// post-refresh semantic state. Only relations and claims touching the
    /// request's documents are included.
    fn build_adjudication_receipt(
        &self,
        request_id: String,
        doc_ids: &[String],
    ) -> WriteAdjudicationReceipt {
        self.build_adjudication_receipt_from_store(&self.store, request_id, doc_ids)
    }

    fn build_adjudication_receipt_from_store(
        &self,
        store: &crate::pipeline::IndexStore,
        request_id: String,
        doc_ids: &[String],
    ) -> WriteAdjudicationReceipt {
        use std::collections::HashSet;
        let doc_set: HashSet<&str> = doc_ids.iter().map(|s| s.as_str()).collect();
        let claims = store
            .semantic_claims()
            .iter()
            .filter(|c| doc_set.contains(c.source_doc_id.as_str()))
            .map(|c| AdjudicatedClaim {
                subject: c.subject.clone(),
                predicate: c.predicate.clone(),
                object: c.object.clone(),
                scope: c.scope.clone(),
                evidence: self.linked_turn_evidence_from_store(store, c),
                confidence: c.confidence,
                assertion_status: c.assertion_status.clone(),
                event_time: c.event_time.clone(),
                claim_evidence: c.claim_evidence.clone(),
            })
            .collect();
        let mut decisions = Vec::new();
        let mut retired_doc_ids = Vec::new();
        for r in store.semantic_relations() {
            let is_source = doc_set.contains(r.source_doc_id.as_str());
            let is_target = doc_set.contains(r.target_doc_id.as_str());
            if !is_source && !is_target {
                continue;
            }
            let kind = match r.kind {
                crate::semantic_relations::SemanticRelationKind::Supersedes => "supersedes",
                crate::semantic_relations::SemanticRelationKind::ConflictsWith => "conflicts_with",
                crate::semantic_relations::SemanticRelationKind::Confirms => "confirms",
            }
            .to_string();
            decisions.push(AdjudicationDecision {
                kind: kind.clone(),
                other_doc_id: if is_source {
                    r.target_doc_id.clone()
                } else {
                    r.source_doc_id.clone()
                },
                is_source,
                confidence: r.confidence,
                method: r.method.clone(),
                evidence: r.evidence.clone(),
            });
            if is_source
                && kind == "supersedes"
                && !doc_set.contains(r.target_doc_id.as_str())
                && !retired_doc_ids.contains(&r.target_doc_id)
            {
                // Report actual retirement, not merely a supersession
                // relation: the target must really be Superseded (not
                // Conflicted via confidence thresholds or the multi-claim
                // protection) and by this write's document.
                let state = store.semantic_document_state(&r.target_doc_id);
                let retired_by_this_write = state.status
                    == Some(crate::semantic_relations::SemanticStatus::Superseded)
                    && state
                        .superseded_by
                        .as_deref()
                        .is_some_and(|id| doc_set.contains(id));
                if retired_by_this_write {
                    retired_doc_ids.push(r.target_doc_id.clone());
                }
            }
        }
        WriteAdjudicationReceipt {
            request_id,
            doc_ids: doc_ids.to_vec(),
            claims,
            decisions,
            retired_doc_ids,
        }
    }

    /// Keep the context document's claim text while pointing to the exact
    /// stored turn span when that evidence appears verbatim in a linked turn.
    /// The existing receipt string carries the citation, so no parallel
    /// evidence DTO is needed.
    fn linked_turn_evidence(&self, claim: &crate::semantic_relations::SemanticClaim) -> String {
        self.linked_turn_evidence_from_store(&self.store, claim)
    }

    fn linked_turn_evidence_from_store(
        &self,
        store: &crate::pipeline::IndexStore,
        claim: &crate::semantic_relations::SemanticClaim,
    ) -> String {
        let Some(context) = store.source_document_by_id(&claim.source_doc_id) else {
            return claim.evidence.clone();
        };
        for turn_id in &context.links {
            let Some(turn) = store.source_document_by_id(turn_id) else {
                continue;
            };
            let Some(start) = turn.content.find(&claim.evidence) else {
                continue;
            };
            let end = start + claim.evidence.len();
            return format!(
                "{} [turn evidence: {} bytes {}..{}]",
                claim.evidence, turn.doc_id, start, end
            );
        }
        claim.evidence.clone()
    }

    fn prepare_add(
        &self,
        request: &mut AddRequest,
        metadata: &BTreeMap<String, serde_json::Value>,
        session_id_was_omitted: bool,
    ) -> anyhow::Result<(String, Vec<SourceDocument>)> {
        validate_identifier(&request.request_id, "request_id")?;
        validate_identifier(&request.user_id, "user_id")?;
        validate_identifier(&request.session_id, "session_id")?;
        if request.messages.is_empty() {
            anyhow::bail!("messages must not be empty");
        }
        if request.messages.len() > MAX_MESSAGES_PER_REQUEST {
            anyhow::bail!("messages must contain at most {MAX_MESSAGES_PER_REQUEST} items");
        }
        for (message_index, message) in request.messages.iter().enumerate() {
            validate_message(message, message_index)?;
        }
        validate_memory_metadata(metadata)?;
        let mut fingerprint =
            request_fingerprint(&request.session_id, &request.messages, metadata)?;
        let request_key = (request.user_id.clone(), request.request_id.clone());
        if let Some(previous) = self.request_fingerprints.get(&request_key) {
            if previous != &fingerprint {
                // Before omitted-session retries used a deterministic session,
                // the client generated a fresh session for each attempt. On a
                // later retry, recover that persisted session only when the
                // request uses today's canonical derived session and the full
                // incoming payload still matches the original fingerprint.
                // Explicit unrelated session changes and changed payloads
                // therefore remain conflicts.
                let canonical_retry_session =
                    crate::memory_client::retry_session_id(&request.user_id, &request.request_id);
                if session_id_was_omitted && request.session_id == canonical_retry_session {
                    if let Some(stored_session) = self
                        .store
                        .source_documents()
                        .into_iter()
                        .filter(|document| {
                            document.filters.get(USER_FILTER) == Some(&request.user_id)
                                && document.filters.get("request_id") == Some(&request.request_id)
                        })
                        .filter_map(|document| document.group_id.clone())
                        .find(|session_id| {
                            request_fingerprint(session_id, &request.messages, metadata)
                                .is_ok_and(|candidate| candidate == *previous)
                        })
                    {
                        request.session_id = stored_session;
                        fingerprint = previous.clone();
                    }
                }
                if previous != &fingerprint {
                    return Err(RequestFingerprintConflict.into());
                }
            }
            return Ok((fingerprint, Vec::new()));
        }

        let mut new_docs = Vec::with_capacity(request.messages.len());
        for (message_index, message) in request.messages.iter().enumerate() {
            let source = format!(
                "memory://{}/{}/{}",
                request.user_id, request.session_id, message_index
            );
            let mut filters = BTreeMap::new();
            filters.insert(USER_FILTER.to_string(), request.user_id.clone());
            filters.insert("request_id".to_string(), request.request_id.clone());
            filters.insert("request_fingerprint".to_string(), fingerprint.clone());
            for (key, value) in metadata {
                filters.insert(key.clone(), encode_metadata_value(value)?);
            }
            if let Some(expires_at_ms) = message.expires_at_ms {
                filters.insert("expires_at_ms".to_string(), expires_at_ms.to_string());
            }
            if let Some(supersedes_id) = &message.supersedes_id {
                validate_identifier(supersedes_id, "supersedes_id")?;
                filters.insert("supersedes_id".to_string(), supersedes_id.clone());
            }
            let timestamp = message
                .timestamp
                .map(|millis| timestamp_to_rfc3339(millis, "timestamp"))
                .transpose()?;
            new_docs.push(SourceDocument {
                doc_id: memory_document_id(&request.user_id, &request.request_id, message_index),
                source,
                content: format!("{}: {}", message.role, message.content).into(),
                concept: "memory".to_string(),
                group_id: Some(request.session_id.clone()),
                headings: vec![],
                links: vec![],
                timestamp,
                doc_length: message.content.len(),
                author_agent: Some(message.role.clone()),
                filters,
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            });
        }
        // Check the whole request before writing any document or supersession.
        // Even a hash collision must never replace another owner's memory or
        // a different request belonging to the same owner.
        for doc in &new_docs {
            if let Some(existing) = self.store.source_document_by_id(&doc.doc_id) {
                anyhow::ensure!(
                    owns_memory(existing, &request.user_id)
                        && existing.filters.get("request_id") == Some(&request.request_id)
                        && existing.filters.get("request_fingerprint") == Some(&fingerprint),
                    "memory document identity conflict"
                );
            }
        }
        Ok((fingerprint, new_docs))
    }

    fn add_unpublished(
        &mut self,
        request: AddRequest,
    ) -> anyhow::Result<(AddResponse, Vec<String>)> {
        self.add_unpublished_with_metadata(AddRequestWithMetadata {
            request,
            metadata: BTreeMap::new(),
        })
    }

    fn add_unpublished_with_metadata(
        &mut self,
        request: AddRequestWithMetadata,
    ) -> anyhow::Result<(AddResponse, Vec<String>)> {
        self.add_unpublished_with_context(AddRequestContext {
            request,
            session_id_was_omitted: false,
        })
    }

    fn add_unpublished_with_context(
        &mut self,
        context: AddRequestContext,
    ) -> anyhow::Result<(AddResponse, Vec<String>)> {
        let omitted_session = context.session_id_was_omitted;
        let (mut request, metadata) = (context.request.request, context.request.metadata);
        let (fingerprint, new_docs) = self.prepare_add(&mut request, &metadata, omitted_session)?;
        let request_key = (request.user_id.clone(), request.request_id.clone());
        // The documents are written; key phrases arrive asynchronously.
        // `upsert` queues each document for background enrichment.
        let doc_ids: Vec<String> = new_docs.iter().map(|d| d.doc_id.clone()).collect();
        for doc in new_docs {
            if let Some(old_id) = doc.filters.get("supersedes_id") {
                self.superseded_ids
                    .insert((request.user_id.clone(), old_id.clone()));
            }
            self.upsert(doc);
        }

        self.request_fingerprints.insert(request_key, fingerprint);
        Ok((
            AddResponse {
                success: true,
                request_id: request.request_id,
                user_id: request.user_id,
                session_id: request.session_id,
                adjudication: None,
            },
            doc_ids,
        ))
    }

    pub fn get(&self, request: GetRequest) -> anyhow::Result<Option<MemoryRecord>> {
        validate_identifier(&request.user_id, "user_id")?;
        validate_identifier(&request.memory_id, "memory_id")?;
        Ok(self
            .store
            .source_document_by_id(&request.memory_id)
            .filter(|doc| owns_memory(doc, &request.user_id))
            .filter(|doc| request.include_inactive || self.is_visible(doc))
            .map(memory_record))
    }

    pub fn list(&self, request: ListRequest) -> anyhow::Result<ListResponse> {
        self.list_with_filter_ast(request, None)
    }

    pub fn list_with_filter_ast(
        &self,
        request: ListRequest,
        filter_ast: Option<&Mem0FilterExpr>,
    ) -> anyhow::Result<ListResponse> {
        if let Some(filter_ast) = filter_ast {
            filter_ast.validate_mem0_fields()?;
        }
        validate_identifier(&request.user_id, "user_id")?;
        if let Some(session_id) = &request.session_id {
            validate_identifier(session_id, "session_id")?;
        }
        let limit = request.limit.clamp(1, 100);
        let mut documents = self
            .store
            .source_documents()
            .into_iter()
            .filter(|doc| owns_memory(doc, &request.user_id))
            .filter(|doc| {
                request.session_id.as_deref().is_none_or(|session| {
                    doc.group_id
                        .as_deref()
                        .map(crate::provider_sources::session_group)
                        == Some(session)
                })
            })
            .filter(|doc| request.include_inactive || self.is_visible(doc))
            .filter(|doc| filter_ast.is_none_or(|ast| ast.matches(doc)))
            .collect::<Vec<_>>();
        let total_count = documents.len();
        documents.sort_by(|left, right| left.doc_id.cmp(&right.doc_id));
        if let Some(cursor) = &request.cursor {
            validate_identifier(cursor, "cursor")?;
            documents.retain(|doc| doc.doc_id > *cursor);
        }
        let has_more = documents.len() > limit;
        documents.truncate(limit);
        let next_cursor = has_more
            .then(|| documents.last().map(|doc| doc.doc_id.clone()))
            .flatten();
        Ok(ListResponse {
            data: documents.into_iter().map(memory_record).collect(),
            next_cursor,
            total_count,
        })
    }

    pub fn update(&mut self, request: UpdateRequest) -> anyhow::Result<Option<MemoryRecord>> {
        self.update_with_metadata(UpdateRequestWithMetadata {
            request,
            metadata: None,
            preserve_expiration: false,
        })
    }

    pub fn update_with_metadata(
        &mut self,
        wrapped: UpdateRequestWithMetadata,
    ) -> anyhow::Result<Option<MemoryRecord>> {
        let UpdateRequestWithMetadata {
            request,
            metadata,
            preserve_expiration,
        } = wrapped;
        validate_identifier(&request.user_id, "user_id")?;
        validate_identifier(&request.memory_id, "memory_id")?;
        let Some(mut document) = self
            .store
            .source_document_by_id(&request.memory_id)
            .cloned()
        else {
            return Ok(None);
        };
        if !owns_memory(&document, &request.user_id) {
            return Ok(None);
        }
        if !self.is_visible(&document) {
            return Ok(None);
        }
        if request.content.trim().is_empty() || request.content.len() > MAX_MESSAGE_CONTENT_BYTES {
            anyhow::bail!(
                "content must be non-empty and at most {MAX_MESSAGE_CONTENT_BYTES} bytes"
            );
        }
        let role = request.role.unwrap_or_else(|| {
            document
                .author_agent
                .clone()
                .unwrap_or_else(|| "user".to_string())
        });
        if role != "user" && role != "assistant" {
            anyhow::bail!("role must be user or assistant");
        }
        document.content = format!("{role}: {}", request.content).into();
        document.author_agent = Some(role);
        document.timestamp = request
            .timestamp
            .map(|millis| timestamp_to_rfc3339(millis, "timestamp"))
            .transpose()?;
        match request.expires_at_ms {
            Some(expires_at) => {
                document
                    .filters
                    .insert("expires_at_ms".to_string(), expires_at.to_string());
            }
            None if !preserve_expiration => {
                document.filters.remove("expires_at_ms");
            }
            None => {}
        }
        if let Some(metadata) = metadata {
            validate_memory_metadata(&metadata)?;
            document
                .filters
                .retain(|key, _| !is_user_metadata_filter(key));
            for (key, value) in metadata {
                document.filters.insert(key, encode_metadata_value(&value)?);
            }
        }
        self.drain_enrichment_inbox();
        // Content changed under the same id: previously extracted phrases
        // describe the old text, so clear them and their extraction stamp;
        // `upsert` re-queues. The stamp must go too: a stale stamp matching
        // new content would make an empty phrase list look "done".
        document.key_phrases = Vec::new();
        document.key_phrase_extraction_hash = String::new();
        self.upsert(document);
        self.flush()?;
        self.append_mutation_audit(
            "update",
            &request.user_id,
            None,
            &[request.memory_id.clone()],
        )?;
        Ok(self
            .store
            .source_document_by_id(&request.memory_id)
            .map(memory_record))
    }

    /// Test-only compatibility path for exercising legacy MCP adapter
    /// fallbacks. Production adapters use the resident server route.
    #[cfg(test)]
    pub(crate) fn sync_shared_memory(
        &mut self,
        memory_root: &std::path::Path,
    ) -> anyhow::Result<bool> {
        workspace::sync_memory_documents(memory_root, self)
    }

    /// Refresh the index for test-only legacy adapter fallbacks.
    pub(crate) fn refresh_index(&mut self) -> anyhow::Result<()> {
        // Provider hooks run refresh_index() on a fresh service: apply any
        // completed background enrichment first, so phrases the worker
        // finished are published by this refresh instead of being stranded
        // in the inbox.
        self.drain_enrichment_inbox();
        self.flush()
    }

    /// Compose workspace and shared captured-memory snapshots into a
    /// single in-memory query view. Transfers already-published segments
    /// rather than reprocessing source documents.
    pub(crate) fn compose_segmented(
        workspace: Self,
        provider_memory: Option<Self>,
    ) -> anyhow::Result<Self> {
        Self::compose_segmented_inner(workspace, provider_memory, None)
    }

    pub(crate) fn compose_segmented_read_view_with_previous(
        workspace: Self,
        provider_memory: Option<Self>,
        previous: crate::IndexStore,
    ) -> anyhow::Result<Self> {
        Self::compose_segmented_inner(workspace, provider_memory, Some(previous))
    }

    fn compose_segmented_inner(
        workspace: Self,
        provider_memory: Option<Self>,
        previous: Option<crate::IndexStore>,
    ) -> anyhow::Result<Self> {
        let provider_generation = provider_memory
            .as_ref()
            .map(|service| service.store.inspection().snapshot_revision)
            .unwrap_or_else(|| workspace.store.inspection().snapshot_revision);
        // The composed view must keep the conversation-state store the hooks
        // write to: the provider memory's disk-backed store (opened under the
        // same `.membrick/memory/` root the hooks use), falling back to the
        // workspace store when there is no provider memory. A fresh
        // memory-only store here would lose hook-written active sessions,
        // drop explicit session state across MCP invocations, and break
        // omitted-session_id inheritance.
        let conversation_states = provider_memory
            .as_ref()
            .map(|service| Arc::clone(&service.conversation_states))
            .unwrap_or_else(|| Arc::clone(&workspace.conversation_states));
        let search_history_index = provider_memory
            .as_ref()
            .map(|service| Arc::clone(&service.search_history_index))
            .unwrap_or_else(|| Arc::clone(&workspace.search_history_index));
        let search_review_state = provider_memory
            .as_ref()
            .map(|service| Arc::clone(&service.search_review_state))
            .unwrap_or_else(|| Arc::clone(&workspace.search_review_state));
        let mut superseded_ids = workspace.superseded_ids.clone();
        let mut request_fingerprints = workspace.request_fingerprints.clone();
        let has_expiring_documents = workspace.has_expiring_documents
            || provider_memory
                .as_ref()
                .is_some_and(|service| service.has_expiring_documents);
        let has_session_observations = workspace.has_session_observations
            || provider_memory
                .as_ref()
                .is_some_and(|service| service.has_session_observations);
        if let Some(provider) = &provider_memory {
            superseded_ids.extend(provider.superseded_ids.iter().cloned());
            request_fingerprints.extend(
                provider
                    .request_fingerprints
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let composed = match previous {
            Some(previous) => crate::IndexStore::compose_segmented_read_view_with_previous(
                workspace.store,
                provider_memory.map(|service| service.store),
                previous,
            )?,
            None => crate::IndexStore::compose_segmented_read_view(
                workspace.store,
                provider_memory.map(|service| service.store),
            )?,
        };
        let mut composed = composed;
        composed.set_published_generation(provider_generation);
        crate::preload_lexical_store();
        Ok(Self {
            store: composed,
            workspace_owner_lock: None,
            workspace_store: None,
            published_search_view: None,
            superseded_ids,
            has_expiring_documents,
            has_session_observations,
            request_fingerprints,
            request_receipts: HashMap::new(),
            receipts_path: None,
            pending_receipt_persistence: HashSet::new(),
            pending_adds: HashMap::new(),
            publication: None,
            conversation_states,
            relations_cache: Mutex::new(RelationsCache::default()),
            enrichment: Arc::new(Mutex::new(KeyPhraseEnrichment::default())),
            project_rules: workspace.project_rules,
            search_history_index,
            search_review_state,
        })
    }

    /// Transfer workspace backing ownership into the resident service. No
    /// external writable workspace handle survives this transfer. The writer
    /// routes commands through this service and publishes both stores together.
    pub(crate) fn retain_workspace_store(mut provider_memory: Self, workspace: Self) -> Self {
        provider_memory.workspace_store = Some(Box::new(workspace));
        provider_memory
    }

    pub(crate) fn has_workspace_store(&self) -> bool {
        self.workspace_store.is_some()
    }

    /// Delete lexical index folders this store's snapshot no longer uses.
    /// Callers must hold the workspace owner lock.
    pub(crate) fn remove_unused_segment_lexical_dirs(&self) -> anyhow::Result<usize> {
        self.store.remove_unused_segment_lexical_dirs()
    }

    pub(crate) fn memory_store(&self, target: MemoryStore) -> &Self {
        match target {
            MemoryStore::Workspace => self.workspace_store.as_deref().unwrap_or(self),
            MemoryStore::Captured => self,
        }
    }

    pub(crate) fn memory_store_mut(&mut self, target: MemoryStore) -> &mut Self {
        match target {
            MemoryStore::Workspace if self.workspace_store.is_some() => self
                .workspace_store
                .as_deref_mut()
                .expect("workspace store present"),
            _ => self,
        }
    }

    pub(crate) fn retain_published_search_view(
        &mut self,
        view: Arc<std::sync::RwLock<Arc<MemoryService>>>,
    ) {
        self.published_search_view = Some(view);
    }

    pub(crate) fn published_search_view(&self) -> anyhow::Result<Option<Arc<MemoryService>>> {
        self.published_search_view
            .as_ref()
            .map(|view| {
                view.read()
                    .map(|view| Arc::clone(&view))
                    .map_err(|_| anyhow::anyhow!("published memory view poisoned"))
            })
            .transpose()
    }

    pub(crate) fn set_published_generation(&mut self, generation: u64) {
        self.store.set_published_generation(generation);
    }

    /// True when the store holds no documents. Integration read paths use
    /// this for the same early exit they had on the raw store.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Insert or replace one source document through `MemoryService`.
    ///
    /// This applies the service's common key-phrase enrichment scheduling
    /// before delegating to its internal `IndexStore`. In-memory callers
    /// publish the changed search view with [`MemoryService::refresh_index`];
    /// durable resident mutations use the service's staged writer path. Agent
    /// adapters must not mutate `IndexStore` directly.
    pub fn upsert(&mut self, document: crate::SourceDocument) {
        // Raw transcript observations duplicate the bounded turn document
        // produced by the same capture. They remain available in the provider
        // transcript, but must never consume semantic, lexical, or segment
        // index memory.
        if is_session_observation(&document) {
            return;
        }
        self.has_expiring_documents |= document.filters.contains_key("expires_at_ms");
        self.queue_key_phrase_enrichment(std::slice::from_ref(&document));
        self.store.upsert(document)
    }

    /// Replace a host-owned capture group, removing obsolete chunks on replay.
    /// Call under the workspace write lock. The replacement filter must match
    /// every new document; an empty filter is never a whole-store delete.
    pub fn replace_source_group(
        &mut self,
        key: &str,
        value: &str,
        documents: Vec<crate::SourceDocument>,
    ) -> anyhow::Result<()> {
        self.replace_source_group_unpublished(key, value, documents)?;
        self.refresh_index()
    }

    fn replace_source_group_unpublished(
        &mut self,
        key: &str,
        value: &str,
        documents: Vec<crate::SourceDocument>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !key.is_empty() && !value.is_empty(),
            "source group identity is required"
        );
        anyhow::ensure!(
            documents
                .iter()
                .all(|document| document.filters.get(key).map(String::as_str) == Some(value)),
            "source group mismatch"
        );
        let live = documents
            .iter()
            .map(|document| document.doc_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let stale = self
            .store
            .source_documents()
            .into_iter()
            .filter(|document| {
                document.filters.get(key).map(String::as_str) == Some(value)
                    && !live.contains(document.doc_id.as_str())
            })
            .map(|document| document.doc_id.clone())
            .collect::<Vec<_>>();
        for id in stale {
            self.drop_enrichment_for_doc(&id);
            self.store.remove(&id);
        }
        for document in documents {
            self.upsert(document);
        }
        Ok(())
    }

    /// Reconcile the workspace-file corpus after the resident server watcher
    /// observes project changes. Provider memories and non-file documents are
    /// left in their own stores and are never removed by this operation.
    pub(crate) fn replace_workspace_documents(
        &mut self,
        documents: Vec<crate::SourceDocument>,
    ) -> anyhow::Result<bool> {
        let workspace_doc_ids = documents
            .iter()
            .map(|document| document.doc_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let existing = self
            .source_documents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut changed = false;
        for document in &existing {
            if is_workspace_markdown_document(document)
                && !workspace_doc_ids.contains(document.doc_id.as_str())
            {
                self.remove(&document.doc_id);
                changed = true;
            }
        }
        for document in documents {
            let unchanged = existing
                .iter()
                .find(|current| current.doc_id == document.doc_id)
                .is_some_and(|current| workspace_documents_equal(current, &document));
            if !unchanged {
                self.upsert(document);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Remove one document by id, returning the removed document if present.
    pub(crate) fn remove(&mut self, doc_id: &str) -> Option<crate::SourceDocument> {
        self.store.remove(doc_id)
    }

    /// Look up a raw index record by document id.
    /// Used by integrations for response formatting, and by core search
    /// for tag-based reranking.
    pub(crate) fn record_by_id(&self, doc_id: &str) -> Option<&crate::index::DocRecord> {
        self.store.record_by_id(doc_id)
    }

    /// All source documents, for migration and sync iteration.
    pub(crate) fn source_documents(&self) -> Vec<&crate::SourceDocument> {
        self.store.source_documents()
    }

    /// Compact status for the workspace-owned reasoning-rule overlay.
    /// `None` means this service was opened as an in-memory/standalone index.
    pub(crate) fn reasoning_rules_status(&self) -> Option<serde_json::Value> {
        let rules = self.project_rules.as_ref()?;
        let status = rules.status();
        Some(serde_json::json!({
            "path": status.path.map(|path| path.display().to_string()),
            "active_query_fingerprint": status.query_fingerprint,
            "active_extraction_fingerprint": status.extraction_fingerprint,
            "last_reload_error": status.last_error,
            "supported_overlay_scope": "query_only",
        }))
    }

    /// Stateless query for integration read paths (recall).
    /// Runs the shared service query pipeline with no session state read or
    /// recorded.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn query_plain(
        &mut self,
        query: &str,
        top_k: usize,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        self.search_with_filters(query, "integration", None, top_k, &BTreeMap::new(), None)
    }

    /// Record the session most recently seen active for `provider` in this
    /// workspace. Hooks call this on every event carrying a session id, and
    /// the MCP search dispatch refreshes it when it resolves a session; the
    /// dispatch reads it back as the default session when the caller did not
    /// pass `session_id` explicitly. Fail-open: the write never fails.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn note_active_session(&self, provider: &str, session_id: &str) {
        if let Ok(store) = self.conversation_states.lock() {
            store.note_active_session(provider, session_id);
        }
    }

    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn deactivate_session(&self, provider: &str, session_id: &str) {
        if let Ok(store) = self.conversation_states.lock() {
            store.deactivate_session(provider, session_id);
        }
    }

    /// The session most recently marked active for `provider`, or `None`
    /// when there is none, it is unreadable, or it is stale. Never fails:
    /// every problem degrades to stateless search.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn current_session_id(&self, provider: &str) -> Option<String> {
        self.conversation_states
            .lock()
            .ok()?
            .current_session_id(provider)
    }

    /// Search for hook context through the shared retrieval pipeline and
    /// record the turn into its session state. Hooks inject background
    /// context rather than answering a conversational turn, but still use
    /// the same query preparation, reranking, facts, and semantic policy as
    /// MCP search. A blank session id degrades to stateless.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn search_for_hook(
        &mut self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        self.search_for_hook_with_comparison_operands(query, scope, session_id, top_k)
            .map(|(results, _)| results)
    }

    pub(crate) fn search_for_hook_with_comparison_operands(
        &mut self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> anyhow::Result<(Vec<crate::SearchResult>, Vec<String>)> {
        self.search_with_filters_and_comparison_operands(
            query,
            scope,
            session_id.filter(|id| !id.trim().is_empty()),
            top_k,
            &BTreeMap::new(),
            None,
        )
    }

    /// Raw record access for tests asserting on capture idempotency and
    /// document attribution. Production code uses the typed service methods.
    #[cfg(test)]
    pub(crate) fn records(&self) -> Vec<&crate::index::DocRecord> {
        self.store.records()
    }

    /// Index-structure access for tests asserting on segment layout.
    #[cfg(test)]
    pub(crate) fn memory_index_snapshot(&self) -> Option<&crate::pipeline::MemoryIndexSnapshot> {
        self.store.memory_index_snapshot()
    }

    /// Number of source documents in the index, for the MCP info tool.
    pub(crate) fn docs_count(&self) -> usize {
        self.store.source_documents().len()
    }

    /// Format search results as the MCP search payload.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn search_results_payload(
        &self,
        query: &str,
        results: Vec<crate::SearchResult>,
    ) -> serde_json::Value {
        crate::integrations::mcp_tools::search_results_for_query(self, query, results)
    }

    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn search_results_payload_with_comparison_operands(
        &self,
        query: &str,
        results: Vec<crate::SearchResult>,
        comparison_operands: &[String],
    ) -> serde_json::Value {
        crate::integrations::mcp_tools::search_results_for_query_with_comparison_operands(
            self,
            query,
            results,
            comparison_operands,
        )
    }

    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn search_agent_cached(
        &self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
        filters: &BTreeMap<String, String>,
    ) -> anyhow::Result<serde_json::Value> {
        let (results, comparison_operands) = self
            .search_with_filters_cached_and_comparison_operands(
                query, scope, session_id, top_k, filters, None,
            )?;
        Ok(
            crate::integrations::mcp_tools::search_results_for_query_with_comparison_operands(
                self,
                query,
                results,
                &comparison_operands,
            ),
        )
    }

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
    pub(crate) fn comparison_operands_for_query(&self, query: &str) -> Vec<String> {
        let rules = self
            .project_rules
            .as_ref()
            .map(|rules| rules.snapshot())
            .unwrap_or_else(crate::reasoning_rules::builtin_rules);
        crate::query_semantics::analyze_query_with_rules(query, rules).comparison_operands
    }

    /// Format the memory list as the MCP list_memories payload.
    #[cfg(any(
        feature = "claude-code",
        feature = "codex",
        feature = "gemini-cli",
        feature = "agy",
        feature = "muse-code",
        feature = "roo-runtime"
    ))]
    pub(crate) fn list_memories_payload(&self, limit: usize) -> serde_json::Value {
        crate::integrations::mcp_tools::list_memories(self, limit)
    }

    /// Look up a source document by id, for tests that verify indexed
    /// content through the service API.
    pub(crate) fn source_document_by_id(&self, doc_id: &str) -> Option<&crate::SourceDocument> {
        self.store.source_document_by_id(doc_id)
    }
    /// Shape raw search results into the API response type. The HTTP server
    /// and language bindings call this after [`MemoryService::search`];
    /// agent-facing callers use the shared `search_results` formatter instead.
    pub fn format_search_response(&self, results: Vec<crate::SearchResult>) -> SearchResponse {
        let data = results
            .into_iter()
            .filter_map(|result| {
                self.store
                    .source_document_by_id(&result.doc_id)
                    .filter(|doc| self.is_visible(doc))
                    .map(|doc| {
                        let record = memory_record(doc);
                        SearchMemory {
                            id: result.doc_id.clone(),
                            content: record.content,
                            score: result.score,
                            created_at: record.created_at,
                            role: record.role,
                            user_id: Some(record.user_id),
                            session_id: record.session_id,
                            metadata: record.metadata,
                            expires_at_ms: record.expires_at_ms,
                            supersedes_id: record.supersedes_id,
                            relation_evidence: result.relation_evidence.clone(),
                            comparison_operands: result.comparison_operands.clone(),
                            claim_states: self.store.semantic_claim_states(&result.doc_id),
                        }
                    })
            })
            .collect();
        SearchResponse { data }
    }

    /// Delete one memory, returning false for an already absent or foreign id.
    pub fn delete(&mut self, user_id: &str, doc_id: &str) -> anyhow::Result<bool> {
        validate_identifier(user_id, "user_id")?;
        validate_identifier(doc_id, "doc_id")?;
        let owned = self
            .store
            .source_document_by_id(doc_id)
            .is_some_and(|doc| doc.filters.get(USER_FILTER).map(String::as_str) == Some(user_id));
        if !owned {
            return Ok(false);
        }
        self.drain_enrichment_inbox();
        self.drop_enrichment_for_doc(doc_id);
        self.store.remove(doc_id);
        self.rebuild_superseded_ids();
        self.flush()?;
        self.append_mutation_audit("delete", user_id, None, &[doc_id.to_string()])?;
        Ok(true)
    }

    /// Rebuild API supersession state from surviving replacement records.
    /// This keeps deletion and expiration behavior consistent with reopening
    /// the store, where this same state is reconstructed from persisted rows.
    fn rebuild_superseded_ids(&mut self) {
        self.superseded_ids = self
            .store
            .source_documents()
            .into_iter()
            .filter_map(|document| {
                Some((
                    document.filters.get(USER_FILTER)?.clone(),
                    document.filters.get("supersedes_id")?.clone(),
                ))
            })
            .collect();
    }

    /// Mark an existing memory as replacing another memory in the same user scope.
    pub fn supersede(
        &mut self,
        user_id: &str,
        replacement_id: &str,
        old_id: &str,
    ) -> anyhow::Result<bool> {
        validate_identifier(user_id, "user_id")?;
        validate_identifier(replacement_id, "replacement_id")?;
        validate_identifier(old_id, "old_id")?;
        let Some(mut replacement) = self.store.source_document_by_id(replacement_id).cloned()
        else {
            return Ok(false);
        };
        if replacement.filters.get(USER_FILTER).map(String::as_str) != Some(user_id) {
            return Ok(false);
        }
        replacement
            .filters
            .insert("supersedes_id".to_string(), old_id.to_string());
        self.superseded_ids
            .insert((user_id.to_string(), old_id.to_string()));
        // Metadata-only touch: the content is unchanged, so the store is
        // written directly without re-queuing key-phrase enrichment.
        self.store.upsert(replacement);
        self.flush()?;
        self.append_mutation_audit(
            "supersede",
            user_id,
            None,
            &[replacement_id.to_string(), old_id.to_string()],
        )?;
        Ok(true)
    }

    /// Remove all expired memories owned by a user.
    pub fn expire(&mut self, user_id: &str) -> anyhow::Result<usize> {
        validate_identifier(user_id, "user_id")?;
        let now_ms = unix_time_ms();
        let ids = self
            .store
            .source_documents()
            .into_iter()
            .filter_map(|doc| {
                let owned = doc.filters.get(USER_FILTER).map(String::as_str) == Some(user_id);
                let expired = doc
                    .filters
                    .get("expires_at_ms")
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_some_and(|expires_at| expires_at <= now_ms);
                (owned && expired).then(|| doc.doc_id.clone())
            })
            .collect::<Vec<_>>();
        for id in &ids {
            self.store.remove(id);
        }
        if !ids.is_empty() {
            self.rebuild_superseded_ids();
            self.flush()?;
            self.append_mutation_audit("expire", user_id, None, &ids)?;
        }
        Ok(ids.len())
    }
}

fn default_list_limit() -> usize {
    100
}

fn owns_memory(doc: &SourceDocument, user_id: &str) -> bool {
    doc.filters.get(USER_FILTER).map(String::as_str) == Some(user_id)
}

/// Versioned, exact identity for new API memories. Length prefixes keep field
/// boundaries unambiguous; identifiers retain their case and whitespace.
/// Existing stored IDs stay valid, and persisted request fingerprints keep
/// retries of legacy requests idempotent without generating a replacement ID.
pub(crate) fn memory_document_id(user_id: &str, request_id: &str, message_index: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(b"lint-ai:memory:v2\0");
    for component in [user_id, request_id] {
        hash.update((component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    hash.update((message_index as u64).to_be_bytes());
    format!("doc:memory:v2:{:x}", hash.finalize())
}

fn memory_record(doc: &SourceDocument) -> MemoryRecord {
    let mut metadata = doc.filters.clone();
    metadata.retain(|key, value| {
        is_user_metadata_filter(key) && value.starts_with(MEMORY_METADATA_PREFIX)
    });
    MemoryRecord {
        id: doc.doc_id.clone(),
        content: doc.content.to_string(),
        role: doc.author_agent.clone(),
        user_id: doc.filters.get(USER_FILTER).cloned().unwrap_or_default(),
        session_id: doc.group_id.clone(),
        created_at: doc.timestamp.clone(),
        expires_at_ms: doc
            .filters
            .get("expires_at_ms")
            .and_then(|value| value.parse().ok()),
        supersedes_id: doc.filters.get("supersedes_id").cloned(),
        metadata,
    }
}

/// Prepare a query against an optional conversation session. Without a
/// session id this is exactly `PreparedQuery::new(query)`; with one, the
/// query is rewritten against the session's bounded prior state (follow-up
/// resolution and temporal-anchor seeding) before analysis.
///
/// `scope` namespaces the state (a user id for the API path, a provider
/// name for the MCP path); `session_id` is the conversation key within it.
/// Shared by `MemoryService` and the MCP dispatchers so both paths get the
/// same stateful behavior.
pub(crate) fn prepare_session_query(
    states: &std::sync::Mutex<crate::conversation_state::ConversationStateStore>,
    scope: &str,
    session_id: Option<&str>,
    query: &str,
    reference_date: Option<&str>,
    rules: std::sync::Arc<crate::reasoning_rules::ReasoningRules>,
) -> PreparedQuery {
    let Some(session_id) = session_id else {
        return PreparedQuery::new_with_rules(query, rules);
    };
    let now_ms = unix_time_ms();
    let rewritten = {
        let mut states = states.lock().expect("conversation state lock poisoned");
        let state = states.get(scope, session_id, now_ms);
        crate::session_prepare::resolve_follow_up(query, state)
    };
    // Reference date (Luyi 2026-10-10 P2): apply to session-scoped searches
    // too, so relative temporal language resolves against it instead of the
    // clock, consistent with non-session searches.
    match reference_date {
        Some(ref_date) => PreparedQuery::new_at_with_rules(&rewritten, ref_date, rules),
        None => PreparedQuery::new_with_rules(&rewritten, rules),
    }
}

/// Record a completed search turn in the session state. No-op without a
/// session id. The original query text is recorded (not the rewritten one),
/// along with the retrieved document ids, entity mentions from the analysis,
/// and the temporal anchor when the query set one explicitly.
pub(crate) fn observe_session_search(
    states: &std::sync::Mutex<crate::conversation_state::ConversationStateStore>,
    scope: &str,
    session_id: Option<&str>,
    original_query: &str,
    prepared: &PreparedQuery,
    results: &[crate::SearchResult],
) {
    let Some(session_id) = session_id else {
        return;
    };
    let analysis = prepared.analysis();
    let entities: Vec<String> = analysis
        .spans
        .iter()
        .map(|span| span.text.clone())
        .collect();
    // `QueryAnalysis` is built before a reference date is applied, so its
    // `QueryTemporal::resolved_at` may be relative to the machine clock.
    // Carry the anchor from the prepared query instead; it has already been
    // resolved against the request's reference date.
    let temporal_anchor = prepared.temporal_context().anchor_date.map(str::to_string);
    let doc_ids: Vec<String> = results.iter().map(|r| r.doc_id.clone()).collect();
    let now_ms = unix_time_ms();
    states
        .lock()
        .expect("conversation state lock poisoned")
        .observe(
            scope,
            session_id,
            original_query,
            &doc_ids,
            &entities,
            temporal_anchor,
            now_ms,
        );
}

/// Entity mentions resolved from the session's recent turns, most recent
/// first. Feeds the conversational rerank's speaker-match feature.
fn session_resolved_entities(
    states: &Mutex<crate::conversation_state::ConversationStateStore>,
    scope: &str,
    session_id: &str,
) -> Vec<String> {
    let now_ms = unix_time_ms();
    states
        .lock()
        .expect("conversation state lock poisoned")
        .get(scope, session_id, now_ms)
        .map(|state| state.resolved_entities.clone())
        .unwrap_or_default()
}

/// True when the conversational rerank should fire for this query: a
/// follow-up inside a live session, with the feature enabled.
fn should_conversational_rerank(enabled: bool, session_id: Option<&str>, query: &str) -> bool {
    enabled && session_id.is_some() && is_follow_up(query)
}

fn request_fingerprint(
    session_id: &str,
    messages: &[Message],
    metadata: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<String> {
    let canonical = if metadata.is_empty() {
        serde_json::to_vec(&(session_id, messages))?
    } else {
        serde_json::to_vec(&(session_id, messages, metadata))?
    };
    let digest = Sha256::digest(canonical);
    Ok(if metadata.is_empty() {
        format!("v2:{digest:x}")
    } else {
        format!("v3:{digest:x}")
    })
}

const MEMORY_METADATA_PREFIX: &str = "mem0-json:";

fn validate_memory_metadata(metadata: &BTreeMap<String, serde_json::Value>) -> anyhow::Result<()> {
    for (key, value) in metadata {
        anyhow::ensure!(
            !key.is_empty()
                && key.len() <= MAX_IDENTIFIER_BYTES
                && !key.chars().any(char::is_control),
            "metadata key must be non-empty, at most {MAX_IDENTIFIER_BYTES} bytes, and contain no control characters"
        );
        anyhow::ensure!(
            is_user_metadata_filter(key),
            "metadata key '{key}' is reserved"
        );
        let encoded = serde_json::to_vec(value)?;
        anyhow::ensure!(
            encoded.len() <= MAX_MESSAGE_CONTENT_BYTES,
            "metadata value for '{key}' is too large"
        );
    }
    Ok(())
}

fn encode_metadata_value(value: &serde_json::Value) -> anyhow::Result<String> {
    Ok(format!(
        "{MEMORY_METADATA_PREFIX}{}",
        serde_json::to_string(value)?
    ))
}

#[cfg(feature = "python")]
pub(crate) fn decode_mem0_metadata(metadata: BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        metadata
            .into_iter()
            .map(|(key, value)| {
                let decoded = value
                    .strip_prefix(MEMORY_METADATA_PREFIX)
                    .and_then(|json| serde_json::from_str(json).ok())
                    .unwrap_or(serde_json::Value::String(value));
                (key, decoded)
            })
            .collect(),
    )
}

#[cfg(feature = "python")]
pub(crate) fn decode_mem0_metadata_value(value: String) -> serde_json::Value {
    value
        .strip_prefix(MEMORY_METADATA_PREFIX)
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or(serde_json::Value::String(value))
}

#[cfg(feature = "python")]
pub(crate) fn encode_mem0_metadata_filter(value: &str) -> String {
    format!(
        "{MEMORY_METADATA_PREFIX}{}",
        serde_json::to_string(&serde_json::Value::String(value.to_owned()))
            .expect("JSON string serialization is infallible")
    )
}

pub(crate) fn is_user_metadata_filter(key: &str) -> bool {
    !key.starts_with("membrick_")
        && !matches!(
            key,
            USER_FILTER
                | "session_id"
                | "request_id"
                | "request_fingerprint"
                | "expires_at_ms"
                | "supersedes_id"
        )
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Identifiers reach the index as filter values and as `memory://` source URIs,
/// so they must stay short, single-line, and free of control characters.
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_MESSAGES_PER_REQUEST: usize = 1024;
const MAX_MESSAGE_CONTENT_BYTES: usize = 1024 * 1024;

fn validate_identifier(value: &str, name: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{name} must not be empty");
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        anyhow::bail!("{name} must be at most {MAX_IDENTIFIER_BYTES} bytes");
    }
    if value.chars().any(|character| character.is_control()) {
        anyhow::bail!("{name} must not contain control characters");
    }
    Ok(())
}

fn validate_message(message: &Message, index: usize) -> anyhow::Result<()> {
    if message.content.trim().is_empty() {
        anyhow::bail!("messages[{index}].content must not be empty");
    }
    if message.content.len() > MAX_MESSAGE_CONTENT_BYTES {
        anyhow::bail!("messages[{index}].content exceeds {MAX_MESSAGE_CONTENT_BYTES} bytes");
    }
    if message.role != "user" && message.role != "assistant" {
        anyhow::bail!("messages[{index}].role must be user or assistant");
    }
    if let Some(supersedes_id) = &message.supersedes_id {
        validate_identifier(supersedes_id, "supersedes_id")?;
    }
    if let Some(millis) = message.timestamp {
        timestamp_to_rfc3339(millis, "timestamp")
            .map_err(|error| anyhow::anyhow!("messages[{index}].{error}"))?;
    }
    Ok(())
}

fn timestamp_to_rfc3339(millis: i64, field: &str) -> anyhow::Result<String> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|date| date.to_rfc3339())
        .ok_or_else(|| anyhow::anyhow!("{field} must be a valid Unix timestamp in milliseconds"))
}

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
impl MemoryService {
    /// Serialize a shared-store mutation against fresh persisted state.
    /// The service owns store selection, lock lifetime, and persistence.
    pub(crate) fn with_shared_memory<T>(
        root: &Path,
        operation: impl FnOnce(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        workspace::with_shared_memory_service(root, operation)
    }

    pub(crate) fn open_workspace(
        root: &Path,
        memory_name: &str,
        ignore_paths: &[String],
        source_documents: impl FnOnce() -> anyhow::Result<Vec<SourceDocument>>,
    ) -> anyhow::Result<Self> {
        let mut service = workspace::open_workspace_memory_store(
            root,
            memory_name,
            ignore_paths,
            source_documents,
        )?;
        service.project_rules = Some(Arc::new(
            crate::reasoning_rules::ProjectReasoningRules::open(root),
        ));
        Ok(service)
    }

    pub(crate) fn open_workspace_owner(
        root: &Path,
        memory_name: &str,
        ignore_paths: &[String],
        source_documents: impl FnOnce() -> anyhow::Result<Vec<SourceDocument>>,
    ) -> anyhow::Result<Self> {
        let mut service = workspace::open_workspace_memory_owner(
            root,
            memory_name,
            ignore_paths,
            source_documents,
        )?;
        service.project_rules = Some(Arc::new(
            crate::reasoning_rules::ProjectReasoningRules::open(root),
        ));
        Ok(service)
    }

    pub(crate) fn open_workspace_owner_with_options_at(
        root: &Path,
        workspace_root: &Path,
        memory_name: &str,
        ignore_paths: &[String],
        options: PipelineOptions,
        source_documents: impl FnOnce() -> anyhow::Result<Vec<SourceDocument>>,
    ) -> anyhow::Result<Self> {
        let mut service = workspace::open_workspace_memory_owner_with_options_at(
            root,
            workspace_root,
            memory_name,
            ignore_paths,
            options,
            source_documents,
        )?;
        service.project_rules = Some(Arc::new(
            crate::reasoning_rules::ProjectReasoningRules::open(root),
        ));
        Ok(service)
    }

    pub(crate) fn open_persistent_workspace(
        root: &Path,
        index_name: &str,
        memory_name: &str,
        ignore_paths: &[String],
        source_documents: impl FnOnce() -> anyhow::Result<Vec<SourceDocument>>,
    ) -> anyhow::Result<Self> {
        workspace::open_persistent_store(
            root,
            index_name,
            memory_name,
            ignore_paths,
            source_documents,
        )
    }

    pub(crate) fn migrate_legacy_memories(root: &Path) -> anyhow::Result<()> {
        workspace::migrate_legacy_provider_memory_dirs(root)
    }
}
