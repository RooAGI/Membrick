use super::{
    attach_bekind_evidence_claims, build_doc_record_with_entities, chunk_lineage_key,
    current_time_ms, doc_record_content_hash, ensure_store_metadata,
    execute_prepared_on_snapshot_parts, inspect_memory_index_snapshot, load_segment_manifest,
    load_semantic_state, persist_segment_manifest, persist_semantic_state, persist_store_metadata,
    rank_key_entities_batched, source_document_from_record, source_documents_to_tier1_inputs,
    IndexLocation, IndexStoreInspection, MemoryIndexLayout, MemoryIndexSnapshot, PipelineOptions,
};
use crate::conversational_rerank::{RerankDocSource, RerankDocView};
use crate::index::{
    build_semantic_doc_state, DocRecord, MemoryIndex, Provenance, QueryDiagnostics, QueryTimings,
    SearchResult, SemanticAggregate, SemanticDocState,
};
use crate::query_plan::PreparedQuery;
use crate::segments::{
    remove_segment_lexical_dirs_when_unused, retire_replaced_segment_lexical_dirs,
    SegmentLexicalDir, SegmentedMemoryIndex,
};
use crate::semantic_relations::SemanticRelationStore;
use crate::source::KeyPhrase;
use crate::source::SourceDocument;
use crate::temporal_fact::TemporalFactStore;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tantivy::doc;
#[derive(Debug, Clone)]
pub struct StorePaths {
    pub root: Option<PathBuf>,
    pub lexical_dir: Option<PathBuf>,
    pub semantic_dir: Option<PathBuf>,
    pub metadata_path: Option<PathBuf>,
}

pub(crate) const STORE_SCHEMA_VERSION: u32 = 1;
pub(crate) const STORE_LAYOUT_VERSION: &str = "index-store-v1";
pub(crate) const SEMANTIC_RECORDS_FILE: &str = "records.json";
pub(crate) const SEMANTIC_CORE_FILE: &str = "core.bin";
pub(crate) const SEGMENT_MANIFEST_FILE: &str = "segments.json";
pub(crate) const CHUNK_LIFECYCLE_FILE: &str = "chunk_lifecycle.json";
pub(crate) const SEMANTIC_RELATIONS_FILE: &str = "relations.bin";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkLifecycleMeta {
    pub chunk_id: String,
    pub doc_id: String,
    pub lineage_key: String,
    pub version: u32,
    pub is_latest: bool,
    pub supersedes_chunk_id: Option<String>,
    pub updated_at_ms: u64,
    pub change_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DocumentLifecycleMeta {
    pub doc_id: String,
    pub version: u32,
    pub is_latest: bool,
    pub updated_at_ms: u64,
    pub chunk_count: usize,
    pub latest_chunk_ids: Vec<String>,
    pub superseded_chunk_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoreMetadata {
    pub(crate) schema_version: u32,
    pub(crate) layout_version: String,
    pub(crate) crate_version: String,
    pub(crate) index_location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedSemanticRecords {
    pub(crate) schema_version: u32,
    pub(crate) layout_version: String,
    pub(crate) records: Vec<PersistedDocRecord>,
    #[serde(default)]
    pub(crate) chunk_lifecycle: Vec<ChunkLifecycleMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedDocRecord {
    doc_id: String,
    source: String,
    content: crate::index::SharedText,
    timestamp: Option<String>,
    doc_length: usize,
    author_agent: Option<String>,
    group_id: Option<String>,
    #[serde(default)]
    filters: std::collections::BTreeMap<String, String>,
    probable_topic: Option<String>,
    doc_type_guess: Option<String>,
    headings: Vec<String>,
    doc_links: Vec<String>,
    #[serde(default)]
    temporal_terms: Vec<String>,
    key_entities: Vec<crate::tier1::Tier1Entity>,
    important_terms: Vec<crate::tier1::RankedTerm>,
    #[serde(default)]
    key_phrases: Vec<KeyPhrase>,
    /// Content hash the key phrases were extracted from; empty means
    /// extraction never completed for the persisted content.
    #[serde(default)]
    key_phrase_extraction_hash: String,
    section_chunks: Vec<crate::index::SectionChunk>,
    embedding: Option<Vec<f32>>,
    top_claims: Vec<crate::index::Claim>,
    provenance: Provenance,
    #[serde(default)]
    content_hash: String,
    #[serde(default)]
    semantic_tags: Vec<String>,
}

impl From<DocRecord> for PersistedDocRecord {
    fn from(record: DocRecord) -> Self {
        Self {
            doc_id: record.doc_id,
            source: record.source,
            content: record.content,
            timestamp: record.timestamp,
            doc_length: record.doc_length,
            author_agent: record.author_agent,
            group_id: record.group_id,
            filters: record.filters,
            probable_topic: record.probable_topic,
            doc_type_guess: record.doc_type_guess,
            headings: record.headings,
            doc_links: record.doc_links,
            temporal_terms: record.temporal_terms,
            key_entities: record.key_entities,
            important_terms: record.important_terms,
            key_phrases: record.key_phrases,
            key_phrase_extraction_hash: record.key_phrase_extraction_hash,
            section_chunks: record.section_chunks,
            embedding: record.embedding,
            top_claims: record.top_claims,
            provenance: record.provenance,
            content_hash: record.content_hash,
            semantic_tags: record.semantic_tags,
        }
    }
}

impl From<PersistedDocRecord> for DocRecord {
    fn from(record: PersistedDocRecord) -> Self {
        let mut record = Self {
            doc_id: record.doc_id,
            source: record.source,
            content: record.content,
            timestamp: record.timestamp,
            doc_length: record.doc_length,
            author_agent: record.author_agent,
            group_id: record.group_id,
            filters: record.filters,
            probable_topic: record.probable_topic,
            doc_type_guess: record.doc_type_guess,
            headings: record.headings,
            doc_links: record.doc_links,
            temporal_terms: record.temporal_terms,
            key_entities: record.key_entities,
            important_terms: record.important_terms,
            key_phrases: record.key_phrases,
            key_phrase_extraction_hash: record.key_phrase_extraction_hash,
            section_chunks: record.section_chunks,
            embedding: record.embedding,
            top_claims: record.top_claims,
            provenance: record.provenance,
            content_hash: record.content_hash,
            semantic_tags: record.semantic_tags,
        };
        record.share_whole_record_chunks();
        record
    }
}

pub fn resolve_store_paths(
    corpus_root: Option<&Path>,
    options: &PipelineOptions,
) -> Result<StorePaths> {
    match &options.index_location {
        IndexLocation::InMemory => Ok(StorePaths {
            root: None,
            lexical_dir: None,
            semantic_dir: None,
            metadata_path: None,
        }),
        IndexLocation::UnderCorpusRoot => {
            let corpus_root = corpus_root
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow::anyhow!("corpus root is required for UnderCorpusRoot"))?;
            let root = corpus_root.join(".membrick");
            Ok(StorePaths {
                lexical_dir: Some(root.join("lexical")),
                semantic_dir: Some(root.join("semantic")),
                metadata_path: Some(root.join("metadata.json")),
                root: Some(root),
            })
        }
        IndexLocation::Explicit(path) => Ok(StorePaths {
            lexical_dir: Some(path.join("lexical")),
            semantic_dir: Some(path.join("semantic")),
            metadata_path: Some(path.join("metadata.json")),
            root: Some(path.clone()),
        }),
    }
}

/// Opaque byte buffers produced by `IndexStore::dump()` and consumed by
/// `IndexStore::load_from_dump()`. Intended for storage in an external system
/// (e.g. Postgres `BYTEA` / `JSONB` columns). Callers should treat the contents
/// as opaque. The schema version embedded in `records_json` guards against
/// version mismatches on restore.
#[derive(Debug, Clone)]
pub struct IndexDump {
    /// JSON-serialized `PersistedSemanticRecords` (doc records + chunk lifecycle).
    pub records_json: Vec<u8>,
    /// Bincode-serialized `PersistedMemoryCore` (BM25 structures, posting lists).
    pub core_bytes: Vec<u8>,
}

/// Cheap staleness hash for background key-phrase enrichment: identifies
/// the exact document content a queued enrichment batch was computed from.
/// Not security-sensitive; only used to drop phrases for documents that
/// were replaced while their batch was in flight.
pub(crate) fn key_phrase_content_hash(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub struct IndexStore {
    options: PipelineOptions,
    pub(crate) store_paths: StorePaths,
    source_docs: Arc<HashMap<String, Arc<SourceDocument>>>,
    records: Arc<HashMap<String, Arc<DocRecord>>>,
    semantic_docs: HashMap<String, SemanticDocState>,
    semantic_aggregate: SemanticAggregate,
    chunk_lifecycle: Arc<HashMap<String, ChunkLifecycleMeta>>,
    chunk_latest_by_lineage: HashMap<String, String>,
    temporal_facts: Arc<TemporalFactStore>,
    semantic_relations: Arc<SemanticRelationStore>,
    dirty_docs: HashSet<String>,
    /// Prepared record changes not yet incorporated into a successful snapshot.
    /// Retained across failed builds, including when content hashes skip NLP.
    pending_snapshot_changes: HashSet<String>,
    tombstones: HashSet<String>,
    snapshot: Option<Arc<MemoryIndexSnapshot>>,
    snapshot_revision: u64,
    checkpoint_revision: u64,
    store_revision: u64,
    dirty: bool,
    checkpoint_pending: bool,
    /// The records whose shards are on disk, so a checkpoint rewrites only
    /// the shards that changed since. `None` writes every shard.
    persisted_records: Option<Arc<HashMap<String, Arc<DocRecord>>>>,
}

pub(crate) struct PreparedCheckpoint {
    /// The records this checkpoint writes, which become the persisted set.
    records: Arc<HashMap<String, Arc<DocRecord>>>,
    semantic: super::PreparedSemanticState,
    manifest: Option<String>,
    /// Relation cache for the same generation, when one could be prepared.
    relations: Option<Vec<u8>>,
    /// Segment catalog cache for the same generation (segmented layouts).
    catalog: Option<Vec<u8>>,
}

/// Write a segmented snapshot's catalog cache for the store under
/// `store_root`. A missing cache only costs the next start a rebuild, so
/// failures are reported, not returned.
fn write_catalog_cache_best_effort(store_root: &Path, bytes: &[u8]) {
    let path = crate::segments::segment_catalog_cache_path(store_root);
    if let Err(error) = crate::segments::write_segment_catalog_cache(&path, bytes) {
        tracing::warn!("could not write segment catalog cache: {error:#}");
    }
}

/// Group members visible to the conversational rerank: documents in `group_id`
/// that also satisfy `filters` (the deep search's user/provider isolation),
/// plus the singleton fallback — stage 1 keys ungrouped documents by doc id,
/// so a group id that names no group but names a document resolves to that
/// document alone instead of vanishing.
fn filtered_group_member_ids(
    source_docs: &std::collections::HashMap<String, Arc<SourceDocument>>,
    group_id: &str,
    filters: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let matches_filters = |doc: &SourceDocument| {
        filters
            .iter()
            .all(|(key, value)| doc.filters.get(key).is_some_and(|actual| actual == value))
    };
    let mut ids: Vec<String> = source_docs
        .values()
        .filter(|doc| doc.group_id.as_deref() == Some(group_id) && matches_filters(doc))
        .map(|doc| doc.doc_id.clone())
        .collect();
    if ids.is_empty() {
        if let Some(doc) = source_docs.get(group_id) {
            if matches_filters(doc) {
                ids.push(doc.doc_id.clone());
            }
        }
    }
    ids
}

/// Shared view construction for the conversational rerank: the indexed text,
/// timestamp for turn ordering, and the author/role as the speaker.
fn rerank_doc_view(doc: &SourceDocument) -> RerankDocView {
    RerankDocView {
        text: doc.content.to_string(),
        timestamp: doc.timestamp.clone(),
        speaker: doc.author_agent.clone(),
    }
}

impl RerankDocSource for IndexStore {
    fn rerank_doc(&self, doc_id: &str) -> Option<RerankDocView> {
        self.source_docs
            .get(doc_id)
            .map(|doc| rerank_doc_view(doc.as_ref()))
    }

    fn group_member_ids(
        &self,
        group_id: &str,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Vec<String> {
        filtered_group_member_ids(&self.source_docs, group_id, filters)
    }
}

fn build_memory_index_snapshot(
    records: Vec<DocRecord>,
    global_index: Option<MemoryIndex>,
    layout: &MemoryIndexLayout,
    generation: u64,
    segment_lexical_root: Option<&Path>,
) -> MemoryIndexSnapshot {
    match layout {
        MemoryIndexLayout::Single => MemoryIndexSnapshot::Single(
            global_index.expect("single-index snapshots require a global index"),
        ),
        MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. } => {
            MemoryIndexSnapshot::Segmented(
                SegmentedMemoryIndex::from_records_by_group_id_with_lexical_root(
                    &records,
                    generation,
                    segment_lexical_root,
                ),
            )
        }
    }
}

/// Called by the writable store when it replaces its snapshot. Detached build
/// views do not call it: their result may still be rejected.
fn retire_replaced_lexical_dirs(
    previous: Option<&MemoryIndexSnapshot>,
    next: Option<&MemoryIndexSnapshot>,
) {
    fn segments(snapshot: Option<&MemoryIndexSnapshot>) -> &[crate::segments::MemoryIndexSegment] {
        match snapshot {
            Some(MemoryIndexSnapshot::Segmented(index)) => &index.segments,
            _ => &[],
        }
    }
    retire_replaced_segment_lexical_dirs(segments(previous), segments(next));
}

fn merge_composed_map<T>(
    target: &mut HashMap<String, T>,
    additions: HashMap<String, T>,
    label: &str,
) -> Result<()> {
    for (id, value) in additions {
        if target.insert(id.clone(), value).is_some() {
            anyhow::bail!("cannot compose stores with duplicate {label} id: {id}");
        }
    }
    Ok(())
}

impl IndexStore {
    pub(crate) fn invalidate_snapshot(&mut self) {
        self.snapshot = None;
        self.dirty = true;
    }

    /// Capture an owned build input. The detached store cannot persist files
    /// and shares only immutable indexes with the mutable owner.
    pub(crate) fn publication_build_view(&mut self) -> Result<Self> {
        if self.dirty || self.snapshot.is_none() {
            self.prepare_pending_changes()?;
            self.pending_snapshot_changes
                .extend(self.tombstones.iter().cloned());
        }
        let mut view = self.published_read_view();
        view.store_revision = self.store_revision;
        view.pending_snapshot_changes = self.pending_snapshot_changes.clone();
        if matches!(self.options.memory_index_layout, MemoryIndexLayout::Single) {
            view.semantic_aggregate = self.semantic_aggregate.clone();
        }
        view.dirty = self.dirty;
        Ok(view)
    }

    /// Adopt a completed generation without replacing mutable records that
    /// may already contain newer writes. Synchronous mutations can supersede it.
    pub(crate) fn accept_publication_view(
        &mut self,
        view: &Self,
        changed: &HashSet<String>,
    ) -> bool {
        if view.snapshot_revision <= self.snapshot_revision {
            return false;
        }
        assert!(view.snapshot_revision <= self.store_revision);
        let previous = std::mem::replace(&mut self.snapshot, view.snapshot.clone());
        retire_replaced_lexical_dirs(previous.as_deref(), self.snapshot.as_deref());
        self.snapshot_revision = view.snapshot_revision;
        for id in changed {
            self.pending_snapshot_changes.remove(id);
        }
        self.dirty = self.store_revision != self.snapshot_revision;
        self.checkpoint_pending = self.snapshot_revision > self.checkpoint_revision;
        true
    }

    pub(crate) fn pending_snapshot_changes(&self) -> HashSet<String> {
        self.pending_snapshot_changes.clone()
    }

    pub(crate) fn checkpoint_revision(&self) -> u64 {
        self.checkpoint_revision
    }

    /// Persist matching index and records from the captured generation.
    /// The MemoryService owner serializes all persistence operations.
    pub(crate) fn checkpoint_publication_view(
        &mut self,
        view: &Self,
        prepared: Option<&PreparedCheckpoint>,
    ) -> Result<()> {
        anyhow::ensure!(
            view.snapshot_revision >= self.checkpoint_revision,
            "checkpoint revision would regress"
        );
        persist_store_metadata(&self.store_paths, &self.options)?;
        if let Some(prepared) = prepared {
            super::persist_prepared_semantic_state(&self.store_paths, &prepared.semantic)?;
            self.persisted_records = Some(prepared.records.clone());
            if let Some(manifest) = &prepared.manifest {
                let path = self
                    .store_paths
                    .semantic_dir
                    .as_ref()
                    .expect("checkpoint semantic dir")
                    .join(SEGMENT_MANIFEST_FILE);
                super::write_text_file_atomic(&path, manifest)?;
            }
            // Written after the records it was fingerprinted against, so a
            // crash in between leaves a cache that no longer matches.
            if let Some(relations) = &prepared.relations {
                if let Err(error) = super::persist_semantic_relations(&self.store_paths, relations)
                {
                    tracing::warn!("could not write semantic relation cache: {error:#}");
                }
            }
            if let (Some(catalog), Some(root)) = (&prepared.catalog, &self.store_paths.root) {
                write_catalog_cache_best_effort(root, catalog);
            }
        } else {
            anyhow::ensure!(
                self.store_paths.semantic_dir.is_none(),
                "disk checkpoint has no prepared payload"
            );
        }
        Ok(())
    }

    pub(crate) fn prepare_checkpoint(&self) -> Result<PreparedCheckpoint> {
        let snapshot = self
            .snapshot
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("checkpoint has no snapshot"))?;
        let (single, manifest, catalog) = match snapshot {
            MemoryIndexSnapshot::Single(index) => (Some(index), None, None),
            MemoryIndexSnapshot::Segmented(index) => {
                let catalog = index.encode_catalog_cache().unwrap_or_else(|error| {
                    tracing::warn!("could not prepare segment catalog cache: {error:#}");
                    Vec::new()
                });
                (
                    None,
                    Some(serde_json::to_string_pretty(&index.manifest())?),
                    (!catalog.is_empty()).then_some(catalog),
                )
            }
        };
        let relations = super::prepare_semantic_relations(
            &self.semantic_relations,
            &self.source_docs,
            &self.records,
            self.options.supersession,
        )
        .unwrap_or_else(|error| {
            tracing::warn!("could not prepare semantic relation cache: {error:#}");
            None
        });
        Ok(PreparedCheckpoint {
            records: self.records.clone(),
            semantic: super::prepare_semantic_state(
                single,
                &self.records,
                &self.chunk_lifecycle,
                self.persisted_records.as_deref(),
            )?,
            manifest,
            relations,
            catalog,
        })
    }

    pub(crate) fn complete_checkpoint(&mut self, revision: u64) {
        self.checkpoint_revision = self.checkpoint_revision.max(revision);
        self.checkpoint_pending = self.snapshot_revision > self.checkpoint_revision;
    }

    /// Delete `segment-lexical` folders the current snapshot does not use.
    /// Each folder holds one immutable version of a segment's lexical index,
    /// so every rebuild of a changed segment leaves the previous version
    /// behind. Only the workspace owner may call this: another process could
    /// still be reading an older version.
    ///
    /// Does nothing unless every segment's folder is known, so a snapshot
    /// with an in-memory segment never causes a live folder to be removed.
    pub(crate) fn remove_unused_segment_lexical_dirs(&self) -> Result<usize> {
        let Some(root) = self.store_paths.root.as_deref() else {
            return Ok(0);
        };
        let lexical_root = root.join("segment-lexical");
        // From now on a replaced folder is deleted as soon as no segment
        // holds it, including folders of segments rebuilt later on.
        remove_segment_lexical_dirs_when_unused(&lexical_root);
        let Some(MemoryIndexSnapshot::Segmented(index)) = self.snapshot.as_deref() else {
            return Ok(0);
        };
        let Some(live) = index
            .segments
            .iter()
            .map(|segment| segment.lexical_dir.as_deref().map(SegmentLexicalDir::name))
            .collect::<Option<HashSet<_>>>()
        else {
            return Ok(0);
        };
        let Ok(entries) = std::fs::read_dir(&lexical_root) else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // Folder names are SHA-256 hex digests; leave anything else alone.
            let is_segment_folder =
                name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit());
            if !is_segment_folder || live.contains(name) || !entry.file_type()?.is_dir() {
                continue;
            }
            std::fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub(crate) fn segment_lexical_dir_names(&self) -> Vec<Option<String>> {
        match self.snapshot.as_deref() {
            Some(MemoryIndexSnapshot::Segmented(index)) => index
                .segments
                .iter()
                .map(|segment| segment.lexical_dir.as_deref().map(SegmentLexicalDir::name))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Detached read view. Shares immutable indexes and copy-on-write corpus
    /// maps, keeping visibility and reranking metadata on one generation.
    pub(crate) fn published_read_view(&self) -> Self {
        Self {
            options: PipelineOptions {
                index_location: IndexLocation::InMemory,
                ..self.options.clone()
            },
            store_paths: StorePaths {
                root: None,
                lexical_dir: None,
                semantic_dir: None,
                metadata_path: None,
            },
            source_docs: self.source_docs.clone(),
            records: self.records.clone(),
            semantic_docs: HashMap::new(),
            semantic_aggregate: SemanticAggregate::default(),
            chunk_lifecycle: self.chunk_lifecycle.clone(),
            chunk_latest_by_lineage: HashMap::new(),
            temporal_facts: self.temporal_facts.clone(),
            semantic_relations: self.semantic_relations.clone(),
            dirty_docs: HashSet::new(),
            pending_snapshot_changes: HashSet::new(),
            tombstones: HashSet::new(),
            snapshot: self.snapshot.clone(),
            snapshot_revision: self.snapshot_revision,
            checkpoint_revision: self.checkpoint_revision,
            store_revision: self.snapshot_revision,
            dirty: false,
            checkpoint_pending: false,
            persisted_records: self.persisted_records.clone(),
        }
    }

    /// Set the generation reported by a detached composed view to the
    /// generation of the writable store whose publication it represents.
    /// Composition creates a fresh in-memory snapshot, so its internal
    /// generation is otherwise always `1` and cannot drive staged barriers.
    pub(crate) fn set_published_generation(&mut self, generation: u64) {
        self.snapshot_revision = generation;
        self.store_revision = generation;
        self.checkpoint_revision = generation;
    }

    #[cfg(test)]
    pub(crate) fn publication_maps_shared_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source_docs, &other.source_docs)
            && Arc::ptr_eq(&self.records, &other.records)
            && Arc::ptr_eq(&self.chunk_lifecycle, &other.chunk_lifecycle)
    }

    /// Wrap a prebuilt single-index query snapshot in the service-owned store
    /// state used by `MemoryService`. This lets adapters that already have a
    /// cached snapshot use the canonical service search path without rebuilding
    /// document records or NLP features.
    pub(crate) fn from_query_index(
        index: MemoryIndex,
        mut options: PipelineOptions,
    ) -> Result<Self> {
        options.index_location = IndexLocation::InMemory;
        options.memory_index_layout = MemoryIndexLayout::Single;
        let records = index
            .docs
            .values()
            .map(|record| PersistedDocRecord::from(record.as_ref().clone()))
            .collect();
        let persisted = PersistedSemanticRecords {
            schema_version: STORE_SCHEMA_VERSION,
            layout_version: STORE_LAYOUT_VERSION.to_string(),
            records,
            chunk_lifecycle: Vec::new(),
        };
        let dump = IndexDump {
            records_json: serde_json::to_vec(&persisted)?,
            core_bytes: index.to_bytes()?,
        };
        Self::load_from_dump(dump, options)
    }

    pub fn new(options: PipelineOptions) -> Self {
        match Self::try_new(options.clone()) {
            Ok(store) => store,
            Err(err) => {
                tracing::warn!(
                    "index store initialization failed (falling back to in-memory): {}",
                    err
                );
                let fallback_options = PipelineOptions {
                    index_location: IndexLocation::InMemory,
                    ..options
                };
                let fallback_paths = StorePaths {
                    root: None,
                    lexical_dir: None,
                    semantic_dir: None,
                    metadata_path: None,
                };
                Self::build_with_store_paths(fallback_options, fallback_paths).unwrap_or_else(
                    |fallback_err| {
                        panic!(
                            "in-memory index store initialization failed after fallback: {}",
                            fallback_err
                        )
                    },
                )
            }
        }
    }

    fn try_new(options: PipelineOptions) -> Result<Self> {
        let store_paths = resolve_store_paths(None, &options)?;
        ensure_store_metadata(&store_paths, &options)?;
        Self::build_with_store_paths(options, store_paths)
    }

    pub fn in_memory(mut options: PipelineOptions) -> Self {
        options.index_location = IndexLocation::InMemory;
        Self::new(options)
    }

    /// Compose already-published segmented stores without rebuilding document
    /// records. This is used by provider MCP servers: the persistent workspace
    /// and a provider's private memory retain their own storage, while the
    /// request process gets one in-memory query view with corpus-wide BM25
    /// statistics.
    pub fn compose_segmented(workspace: Self, provider_memory: Option<Self>) -> Result<Self> {
        Self::compose_segmented_inner(workspace, provider_memory, true, None)
    }

    /// Published views query frozen snapshots and never need writer-only postings.
    pub(crate) fn compose_segmented_read_view(
        workspace: Self,
        provider_memory: Option<Self>,
    ) -> Result<Self> {
        Self::compose_segmented_inner(workspace, provider_memory, false, None)
    }

    pub(crate) fn compose_segmented_read_view_with_previous(
        workspace: Self,
        provider_memory: Option<Self>,
        previous: Self,
    ) -> Result<Self> {
        Self::compose_segmented_inner(workspace, provider_memory, false, Some(previous))
    }

    fn compose_segmented_inner(
        workspace: Self,
        provider_memory: Option<Self>,
        writer_metadata: bool,
        previous: Option<Self>,
    ) -> Result<Self> {
        if !matches!(
            workspace.options.memory_index_layout,
            MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. }
        ) {
            anyhow::bail!("composed stores require a segmented memory index layout");
        }
        if provider_memory.as_ref().is_some_and(|provider| {
            !matches!(
                provider.options.memory_index_layout,
                MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. }
            )
        }) {
            anyhow::bail!("provider memory must use a segmented memory index layout");
        }

        let workspace_segmented = match workspace.snapshot.as_deref() {
            Some(MemoryIndexSnapshot::Segmented(index)) => Some(index),
            Some(MemoryIndexSnapshot::Single(_)) => {
                anyhow::bail!("composed stores require segmented snapshots")
            }
            None => None,
        };
        let provider_segmented = match provider_memory
            .as_ref()
            .and_then(|provider| provider.snapshot.as_deref())
        {
            Some(MemoryIndexSnapshot::Segmented(index)) => Some(index),
            Some(MemoryIndexSnapshot::Single(_)) => {
                anyhow::bail!("provider memory must use a segmented snapshot")
            }
            None => None,
        };
        let empty_segmented = SegmentedMemoryIndex::from_segments_with_generation(Vec::new(), 0)
            .map_err(anyhow::Error::msg)?;
        let workspace_segmented = workspace_segmented.unwrap_or(&empty_segmented);
        let snapshot = if workspace_segmented.segments.is_empty()
            && provider_segmented.is_none_or(|index| index.segments.is_empty())
        {
            None
        } else {
            Some(Arc::new(MemoryIndexSnapshot::Segmented(
                SegmentedMemoryIndex::compose(workspace_segmented, provider_segmented, 1)
                    .map_err(anyhow::Error::msg)?,
            )))
        };

        let IndexStore {
            source_docs,
            records,
            chunk_lifecycle,
            options,
            semantic_relations: workspace_relations,
            dirty: workspace_dirty,
            ..
        } = workspace;
        let mut source_docs = Arc::unwrap_or_clone(source_docs);
        let mut records = Arc::unwrap_or_clone(records);
        let mut chunk_lifecycle = Arc::unwrap_or_clone(chunk_lifecycle);
        // Each side already holds relations for its own documents. Start from
        // the larger side and add the other side's documents incrementally,
        // instead of rebuilding relations for the whole union.
        let mut reusable_relations = None;
        if let Some(provider_memory) = provider_memory {
            let IndexStore {
                source_docs: provider_docs,
                records: provider_records,
                chunk_lifecycle: provider_lifecycle,
                options: provider_options,
                semantic_relations: provider_relations,
                dirty: provider_dirty,
                ..
            } = provider_memory;
            let provider_is_base = provider_docs.len() >= source_docs.len();
            let (base_relations, base_dirty, base_options, added_ids) = if provider_is_base {
                (
                    provider_relations,
                    provider_dirty,
                    provider_options.supersession,
                    source_docs.keys().cloned().collect::<Vec<_>>(),
                )
            } else {
                (
                    workspace_relations,
                    workspace_dirty,
                    options.supersession,
                    provider_docs.keys().cloned().collect::<Vec<_>>(),
                )
            };
            if !base_dirty && base_options == options.supersession {
                reusable_relations = Some((base_relations, added_ids));
            }
            merge_composed_map(
                &mut source_docs,
                Arc::unwrap_or_clone(provider_docs),
                "source document",
            )?;
            merge_composed_map(
                &mut records,
                Arc::unwrap_or_clone(provider_records),
                "record",
            )?;
            merge_composed_map(
                &mut chunk_lifecycle,
                Arc::unwrap_or_clone(provider_lifecycle),
                "chunk lifecycle",
            )?;
        }

        let mut semantic_docs = HashMap::new();
        let mut semantic_aggregate = SemanticAggregate::default();
        let mut chunk_latest_by_lineage = HashMap::new();
        if writer_metadata {
            if matches!(options.memory_index_layout, MemoryIndexLayout::Single) {
                for record in records.values().map(AsRef::as_ref) {
                    let state = build_semantic_doc_state(record, options.claim_extraction);
                    semantic_aggregate.insert_doc_state(&state);
                    semantic_docs.insert(record.doc_id.clone(), state);
                }
            }
            for meta in chunk_lifecycle.values().filter(|meta| meta.is_latest) {
                chunk_latest_by_lineage.insert(meta.lineage_key.clone(), meta.chunk_id.clone());
            }
        }
        let temporal_facts = if let Some(previous) = previous {
            let IndexStore {
                records: previous_records,
                temporal_facts: previous_facts,
                ..
            } = previous;
            TemporalFactStore::carry_forward(
                previous_facts,
                &previous_records,
                &records,
                &chunk_lifecycle,
            )
        } else {
            Arc::new(TemporalFactStore::from_records(
                records.values().map(AsRef::as_ref),
                &chunk_lifecycle,
            ))
        };
        let semantic_relations = match reusable_relations {
            Some((relations, added_ids)) => {
                let mut relations = Arc::unwrap_or_clone(relations);
                let added = added_ids
                    .iter()
                    .filter_map(|doc_id| source_docs.get(doc_id).map(AsRef::as_ref))
                    .collect::<Vec<_>>();
                relations.update_documents_borrowed(
                    &source_docs,
                    &added,
                    &[],
                    options.supersession,
                )?;
                relations
            }
            None => SemanticRelationStore::try_from_documents(
                source_docs.values().map(AsRef::as_ref),
                options.supersession,
            )?,
        };

        Ok(Self {
            options: PipelineOptions {
                index_location: IndexLocation::InMemory,
                ..options
            },
            store_paths: StorePaths {
                root: None,
                lexical_dir: None,
                semantic_dir: None,
                metadata_path: None,
            },
            source_docs: Arc::new(source_docs),
            records: Arc::new(records),
            semantic_docs,
            semantic_aggregate,
            chunk_lifecycle: Arc::new(chunk_lifecycle),
            chunk_latest_by_lineage,
            temporal_facts,
            semantic_relations: Arc::new(semantic_relations),
            dirty_docs: HashSet::new(),
            pending_snapshot_changes: HashSet::new(),
            tombstones: HashSet::new(),
            snapshot_revision: usize::from(snapshot.is_some()) as u64,
            checkpoint_revision: 0,
            store_revision: usize::from(snapshot.is_some()) as u64,
            snapshot,
            dirty: false,
            checkpoint_pending: false,
            persisted_records: None,
        })
    }

    pub fn for_corpus(corpus_root: &Path, mut options: PipelineOptions) -> Result<Self> {
        options.index_location = IndexLocation::UnderCorpusRoot;
        let store_paths = resolve_store_paths(Some(corpus_root), &options)?;
        ensure_store_metadata(&store_paths, &options)?;
        Self::build_with_store_paths(options, store_paths)
    }

    pub fn at_path(index_root: &Path, mut options: PipelineOptions) -> Result<Self> {
        options.index_location = IndexLocation::Explicit(index_root.to_path_buf());
        let store_paths = resolve_store_paths(None, &options)?;
        ensure_store_metadata(&store_paths, &options)?;
        Self::build_with_store_paths(options, store_paths)
    }

    fn build_with_store_paths(options: PipelineOptions, store_paths: StorePaths) -> Result<Self> {
        let load_started = Instant::now();
        let store_path = store_paths
            .root
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<in-memory>".to_string());
        let file_size = |path: Option<PathBuf>| {
            path.and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let records_file_bytes = file_size(
            store_paths
                .semantic_dir
                .as_deref()
                .map(|path| path.join(SEMANTIC_RECORDS_FILE)),
        );
        let core_file_bytes = file_size(
            store_paths
                .semantic_dir
                .as_deref()
                .map(|path| path.join(SEMANTIC_CORE_FILE)),
        );
        let manifest_file_bytes = file_size(
            store_paths
                .root
                .as_deref()
                .map(|path| path.join(SEGMENT_MANIFEST_FILE)),
        );
        let relations_file_bytes = file_size(
            store_paths
                .semantic_dir
                .as_deref()
                .map(|path| path.join(SEMANTIC_RELATIONS_FILE)),
        );
        let state_load_started = Instant::now();
        let (source_docs, records, chunk_lifecycle, loaded_snapshot, from_shards) =
            load_semantic_state(&store_paths, &options.memory_index_layout)?;
        // Taken before any change to the loaded records, so it matches the
        // shards on disk.
        let persisted_records = from_shards.then(|| Arc::new(records.clone()));
        let state_load_ms = state_load_started.elapsed().as_millis() as u64;
        let source_content_bytes = source_docs
            .values()
            .map(|document| document.content.len())
            .sum::<usize>();
        let record_content_bytes = records
            .values()
            .map(|record| record.content.len())
            .sum::<usize>();
        let segment_lexical_root = store_paths
            .root
            .as_deref()
            .map(|root| root.join("segment-lexical"));
        let had_loaded_single_snapshot = loaded_snapshot.is_some();
        let snapshot_load_started = Instant::now();
        let mut reused_segment_manifest = false;
        let snapshot = if let Some(global) = loaded_snapshot {
            let records = records
                .values()
                .map(AsRef::as_ref)
                .cloned()
                .collect::<Vec<_>>();
            Some(build_memory_index_snapshot(
                records,
                Some(global),
                &options.memory_index_layout,
                0,
                segment_lexical_root.as_deref(),
            ))
        } else if matches!(
            options.memory_index_layout,
            MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. }
        ) && !records.is_empty()
        {
            let shared_records = records.values().cloned().collect::<Vec<_>>();
            let manifest = load_segment_manifest(&store_paths)?;
            let snapshot = manifest.and_then(|manifest| {
                match SegmentedMemoryIndex::from_shared_records_by_manifest_with_lexical_root(
                    &shared_records,
                    &manifest,
                    0,
                    segment_lexical_root.as_deref(),
                ) {
                    Ok(segmented) => Some(MemoryIndexSnapshot::Segmented(segmented)),
                    Err(error) => {
                        tracing::warn!("ignoring invalid persisted segment manifest: {error}");
                        None
                    }
                }
            });
            reused_segment_manifest = snapshot.is_some();
            Some(snapshot.unwrap_or_else(|| {
                let records = records
                    .values()
                    .map(|record| record.as_ref().clone())
                    .collect::<Vec<_>>();
                build_memory_index_snapshot(
                    records,
                    None,
                    &options.memory_index_layout,
                    0,
                    segment_lexical_root.as_deref(),
                )
            }))
        } else {
            None
        };
        let snapshot_load_ms = snapshot_load_started.elapsed().as_millis() as u64;
        let snapshot_kind = match snapshot.as_ref() {
            Some(MemoryIndexSnapshot::Single(_)) if had_loaded_single_snapshot => "single_core",
            Some(MemoryIndexSnapshot::Single(_)) => "single_rebuilt",
            Some(MemoryIndexSnapshot::Segmented(_)) if reused_segment_manifest => {
                "segmented_manifest"
            }
            Some(MemoryIndexSnapshot::Segmented(_)) => "segmented_rebuilt",
            None => "empty",
        };
        let snapshot_segments = match snapshot.as_ref() {
            Some(MemoryIndexSnapshot::Segmented(index)) => index.segments.len(),
            Some(MemoryIndexSnapshot::Single(_)) | None => 0,
        };
        let mut semantic_docs = HashMap::new();
        let mut semantic_aggregate = SemanticAggregate::default();
        let mut chunk_latest_by_lineage = HashMap::new();
        let temporal_load_started = Instant::now();
        let temporal_facts =
            TemporalFactStore::from_records(records.values().map(AsRef::as_ref), &chunk_lifecycle);
        let temporal_load_ms = temporal_load_started.elapsed().as_millis() as u64;
        let relations_load_started = Instant::now();
        let mut loaded_relations_cache = true;
        let semantic_relations =
            match super::load_semantic_relations(&store_paths, &records, options.supersession) {
                Some(cached) => cached,
                None => {
                    loaded_relations_cache = false;
                    let built = SemanticRelationStore::try_from_documents(
                        source_docs.values().map(AsRef::as_ref),
                        options.supersession,
                    )?;
                    if !records.is_empty() {
                        super::persist_semantic_relations_best_effort(
                            &store_paths,
                            &built,
                            &source_docs,
                            &records,
                            options.supersession,
                        );
                    }
                    built
                }
            };
        let relations_load_ms = relations_load_started.elapsed().as_millis() as u64;
        // Segmented snapshots derive postings inside each segment. A second
        // corpus-wide set is used only by the single-index compatibility path.
        if matches!(options.memory_index_layout, MemoryIndexLayout::Single) {
            for record in records.values().map(AsRef::as_ref) {
                let state = build_semantic_doc_state(record, options.claim_extraction);
                semantic_aggregate.insert_doc_state(&state);
                semantic_docs.insert(record.doc_id.clone(), state);
            }
        }
        for meta in chunk_lifecycle.values().filter(|meta| meta.is_latest) {
            chunk_latest_by_lineage.insert(meta.lineage_key.clone(), meta.chunk_id.clone());
        }
        tracing::info!(
            target: "membrick::memory_load",
            store = %store_path,
            layout = ?options.memory_index_layout,
            source_documents = source_docs.len(),
            source_content_bytes,
            records = records.len(),
            record_content_bytes,
            chunk_lifecycle_entries = chunk_lifecycle.len(),
            records_file_bytes,
            core_file_bytes,
            segment_manifest_file_bytes = manifest_file_bytes,
            semantic_relations_file_bytes = relations_file_bytes,
            snapshot_kind,
            snapshot_segments,
            semantic_relations_cache = if loaded_relations_cache { "loaded" } else { "rebuilt" },
            state_load_ms,
            snapshot_load_ms,
            temporal_load_ms,
            relations_load_ms,
            total_load_ms = load_started.elapsed().as_millis() as u64,
            "memory store loaded"
        );
        Ok(Self {
            options,
            store_paths,
            source_docs: Arc::new(source_docs),
            records: Arc::new(records),
            semantic_docs,
            semantic_aggregate,
            chunk_lifecycle: Arc::new(chunk_lifecycle),
            chunk_latest_by_lineage,
            temporal_facts: Arc::new(temporal_facts),
            semantic_relations: Arc::new(semantic_relations),
            dirty_docs: HashSet::new(),
            pending_snapshot_changes: HashSet::new(),
            tombstones: HashSet::new(),
            snapshot: snapshot.map(Arc::new),
            snapshot_revision: 0,
            checkpoint_revision: 0,
            store_revision: 0,
            dirty: false,
            checkpoint_pending: false,
            persisted_records,
        })
    }

    pub fn with_documents(options: PipelineOptions, docs: Vec<SourceDocument>) -> Self {
        let mut index = Self::new(options);
        for doc in docs {
            index.upsert(doc);
        }
        index
    }

    fn build_compatibility_index(&mut self) -> MemoryIndex {
        let mut records = self
            .records
            .values()
            .map(AsRef::as_ref)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.doc_id.cmp(&right.doc_id));
        if !matches!(self.options.memory_index_layout, MemoryIndexLayout::Single) {
            // Export/dump may ask a segmented store for a compatibility index.
            // Build its global postings only for this request, then release them.
            return MemoryIndex::from_records_with_lexical_dir(
                records,
                None,
                self.options.text_rerank_ngram,
                self.options.text_rerank_lcs,
                self.options.claim_extraction,
            );
        }
        self.semantic_aggregate.sort_pending_postings();
        MemoryIndex::from_records_with_semantic_aggregate(
            records,
            self.semantic_aggregate.clone(),
            self.options.text_rerank_ngram,
            self.options.text_rerank_lcs,
            self.options.claim_extraction,
        )
    }

    fn persist_compatibility_state(&mut self) -> Result<()> {
        let paths = &self.store_paths;
        if paths.semantic_dir.is_none() {
            return Ok(());
        }
        let persisted = self.persisted_records.as_deref();
        match self.snapshot.as_deref().expect("snapshot should exist") {
            MemoryIndexSnapshot::Single(index) => persist_semantic_state(
                paths,
                Some(index),
                &self.records,
                &self.chunk_lifecycle,
                persisted,
            )?,
            MemoryIndexSnapshot::Segmented(segmented) => {
                persist_semantic_state(
                    paths,
                    None,
                    &self.records,
                    &self.chunk_lifecycle,
                    persisted,
                )?;
                persist_segment_manifest(paths, &segmented.manifest())?;
                if let Some(root) = &paths.root {
                    match segmented.encode_catalog_cache() {
                        Ok(bytes) => write_catalog_cache_best_effort(root, &bytes),
                        Err(error) => {
                            tracing::warn!("could not prepare segment catalog cache: {error:#}")
                        }
                    }
                }
            }
        }
        self.persisted_records = Some(self.records.clone());
        super::persist_semantic_relations_best_effort(
            paths,
            &self.semantic_relations,
            &self.source_docs,
            &self.records,
            self.options.supersession,
        );
        Ok(())
    }

    /// Serialize the current index state to a pair of opaque byte buffers suitable
    /// for storage in an external system (e.g. Postgres BYTEA / JSONB columns).
    /// Call `load_from_dump` to restore.
    pub(crate) fn dump(&mut self) -> Result<IndexDump> {
        self.refresh()?;
        let compatibility_index = self.build_compatibility_index();
        let core_bytes = compatibility_index.to_bytes()?;
        let records: Vec<PersistedDocRecord> = self
            .records
            .values()
            .map(|r| r.as_ref().clone())
            .map(PersistedDocRecord::from)
            .collect();
        let chunk_lifecycle: Vec<ChunkLifecycleMeta> =
            self.chunk_lifecycle.values().cloned().collect();
        let persisted = PersistedSemanticRecords {
            schema_version: STORE_SCHEMA_VERSION,
            layout_version: STORE_LAYOUT_VERSION.to_string(),
            records,
            chunk_lifecycle,
        };
        let records_json = serde_json::to_vec(&persisted)?;
        Ok(IndexDump {
            records_json,
            core_bytes,
        })
    }

    /// Restore an `IndexStore` from a dump produced by `dump()`.
    /// Returns an error if the schema version does not match.
    pub(crate) fn load_from_dump(dump: IndexDump, options: PipelineOptions) -> Result<Self> {
        let persisted: PersistedSemanticRecords = serde_json::from_slice(&dump.records_json)?;
        if persisted.schema_version != STORE_SCHEMA_VERSION {
            anyhow::bail!(
                "index dump schema mismatch: found {}, expected {}",
                persisted.schema_version,
                STORE_SCHEMA_VERSION
            );
        }
        if persisted.layout_version != STORE_LAYOUT_VERSION {
            anyhow::bail!(
                "index dump layout mismatch: found {}, expected {}",
                persisted.layout_version,
                STORE_LAYOUT_VERSION
            );
        }
        let restored_records: Vec<DocRecord> =
            persisted.records.into_iter().map(Into::into).collect();
        let global_index = MemoryIndex::from_bytes(&dump.core_bytes, restored_records.clone())?;
        let snapshot = build_memory_index_snapshot(
            restored_records.clone(),
            Some(global_index),
            &options.memory_index_layout,
            0,
            None,
        );
        let mut source_docs = HashMap::new();
        let mut records = HashMap::new();
        let mut chunk_lifecycle: HashMap<String, ChunkLifecycleMeta> = persisted
            .chunk_lifecycle
            .into_iter()
            .map(|m| (m.chunk_id.clone(), m))
            .collect();
        for record in &restored_records {
            source_docs.insert(
                record.doc_id.clone(),
                Arc::new(source_document_from_record(record)),
            );
            records.insert(record.doc_id.clone(), Arc::new(record.clone()));
        }
        if chunk_lifecycle.is_empty() {
            for record in records.values().map(AsRef::as_ref) {
                for chunk in &record.section_chunks {
                    chunk_lifecycle.insert(
                        chunk.chunk_id.clone(),
                        ChunkLifecycleMeta {
                            chunk_id: chunk.chunk_id.clone(),
                            doc_id: record.doc_id.clone(),
                            lineage_key: chunk_lineage_key(&record.doc_id, chunk),
                            version: 1,
                            is_latest: true,
                            supersedes_chunk_id: None,
                            updated_at_ms: current_time_ms(),
                            change_reason: Some("bootstrap".to_string()),
                        },
                    );
                }
            }
        }
        let chunk_latest_by_lineage = chunk_lifecycle
            .values()
            .filter(|m| m.is_latest)
            .map(|m| (m.lineage_key.clone(), m.chunk_id.clone()))
            .collect();
        let temporal_facts =
            TemporalFactStore::from_records(records.values().map(AsRef::as_ref), &chunk_lifecycle);
        let semantic_relations = SemanticRelationStore::try_from_documents(
            source_docs.values().map(AsRef::as_ref),
            options.supersession,
        )?;
        let mut semantic_docs = HashMap::new();
        let mut semantic_aggregate = SemanticAggregate::default();
        // Segmented snapshots derive postings inside each segment. A second
        // corpus-wide set is used only by the single-index compatibility path.
        if matches!(options.memory_index_layout, MemoryIndexLayout::Single) {
            for record in records.values().map(AsRef::as_ref) {
                let state = build_semantic_doc_state(record, options.claim_extraction);
                semantic_aggregate.insert_doc_state(&state);
                semantic_docs.insert(record.doc_id.clone(), state);
            }
        }
        let store_paths = StorePaths {
            root: None,
            lexical_dir: None,
            semantic_dir: None,
            metadata_path: None,
        };
        Ok(Self {
            options,
            store_paths,
            source_docs: Arc::new(source_docs),
            records: Arc::new(records),
            semantic_docs,
            semantic_aggregate,
            chunk_lifecycle: Arc::new(chunk_lifecycle),
            chunk_latest_by_lineage,
            temporal_facts: Arc::new(temporal_facts),
            semantic_relations: Arc::new(semantic_relations),
            dirty_docs: HashSet::new(),
            pending_snapshot_changes: HashSet::new(),
            tombstones: HashSet::new(),
            snapshot: Some(Arc::new(snapshot)),
            snapshot_revision: 1,
            checkpoint_revision: 0,
            store_revision: 1,
            dirty: false,
            checkpoint_pending: false,
            persisted_records: None,
        })
    }

    pub fn upsert(&mut self, doc: SourceDocument) {
        let mut doc = doc;
        doc.normalize_timestamp_metadata();
        const RECORDED_AT_FILTER: &str = "membrick_recorded_at";
        let doc_id = doc.doc_id.clone();
        let recorded_at = self.source_docs.get(&doc_id).and_then(|previous| {
            let unchanged = previous.content == doc.content
                && previous.source == doc.source
                && previous.timestamp == doc.timestamp;
            unchanged
                .then(|| previous.filters.get(RECORDED_AT_FILTER).cloned())
                .flatten()
        });
        let recorded_at = recorded_at.unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });
        doc.filters
            .insert(RECORDED_AT_FILTER.to_string(), recorded_at);
        self.tombstones.remove(&doc_id);
        Arc::make_mut(&mut self.source_docs).insert(doc_id.clone(), Arc::new(doc));
        self.dirty_docs.insert(doc_id);
        self.store_revision = self.store_revision.saturating_add(1);
        self.dirty = true;
    }

    /// Backfill grammar-accepted entity key phrases for a document
    /// (background enrichment on the write path).
    ///
    /// The update applies only when `expected_content_hash` matches the
    /// document's current content: a document replaced while its
    /// enrichment batch was in flight keeps its newer state, and the
    /// replacement re-queued itself via `add`. Matching documents are
    /// marked dirty so the next refresh rebuilds their records — the
    /// record content hash covers `key_phrases`, so the rebuild is not
    /// skipped as a no-op.
    pub fn set_key_phrases(
        &mut self,
        doc_id: &str,
        expected_content_hash: &str,
        key_phrases: Vec<KeyPhrase>,
    ) {
        let current_matches = self
            .source_docs
            .get(doc_id)
            .is_some_and(|doc| key_phrase_content_hash(&doc.content) == expected_content_hash);
        if !current_matches {
            return;
        }
        if let Some(doc) = Arc::make_mut(&mut self.source_docs).get_mut(doc_id) {
            let doc = Arc::make_mut(doc);
            let mut merged = key_phrases;
            for phrase in doc.key_phrases.iter().filter(|phrase| {
                phrase.origin == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
            }) {
                if !merged.iter().any(|existing| {
                    existing.text.eq_ignore_ascii_case(&phrase.text)
                        && existing.kind.eq_ignore_ascii_case(&phrase.kind)
                }) {
                    merged.push(phrase.clone());
                }
            }
            doc.key_phrases = merged;
            // Stamp the content this extraction ran against: an empty phrase
            // list with a matching stamp is a completed extraction, not a
            // document still waiting for one.
            doc.key_phrase_extraction_hash = expected_content_hash.to_string();
        }
        self.dirty_docs.insert(doc_id.to_string());
        self.store_revision = self.store_revision.saturating_add(1);
        self.dirty = true;
    }

    pub fn remove(&mut self, doc_id: &str) -> Option<SourceDocument> {
        let removed = Arc::make_mut(&mut self.source_docs).remove(doc_id);
        if removed.is_some() {
            Arc::make_mut(&mut self.records).remove(doc_id);
            self.semantic_docs.remove(doc_id);
            self.semantic_aggregate.remove_doc(doc_id);
            self.dirty_docs.remove(doc_id);
            self.tombstones.insert(doc_id.to_string());
            self.store_revision = self.store_revision.saturating_add(1);
            self.dirty = true;
        }
        removed.map(Arc::unwrap_or_clone)
    }

    /// Remove many documents with one aggregate pass. Returns how many existed.
    pub fn remove_many<'a>(&mut self, doc_ids: impl IntoIterator<Item = &'a str>) -> usize {
        let source_docs = Arc::make_mut(&mut self.source_docs);
        let records = Arc::make_mut(&mut self.records);
        let mut removed = Vec::new();
        for doc_id in doc_ids {
            if source_docs.remove(doc_id).is_some() {
                records.remove(doc_id);
                self.semantic_docs.remove(doc_id);
                self.dirty_docs.remove(doc_id);
                self.tombstones.insert(doc_id.to_string());
                removed.push(doc_id);
            }
        }
        if !removed.is_empty() {
            self.semantic_aggregate.remove_docs(removed.iter().copied());
            self.store_revision = self.store_revision.saturating_add(1);
            self.dirty = true;
        }
        removed.len()
    }

    pub fn len(&self) -> usize {
        self.source_docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source_docs.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[allow(dead_code)]
    pub(crate) fn store_revision(&self) -> u64 {
        self.store_revision
    }

    #[allow(dead_code)]
    pub(crate) fn snapshot_revision(&self) -> u64 {
        self.snapshot_revision
    }

    pub fn tombstones(&self) -> Vec<&str> {
        let mut tombstones = self
            .tombstones
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        tombstones.sort_unstable();
        tombstones
    }

    pub fn source_documents(&self) -> Vec<&SourceDocument> {
        let mut docs: Vec<&SourceDocument> = self.source_docs.values().map(AsRef::as_ref).collect();
        docs.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        docs
    }

    pub fn source_document_by_id(&self, doc_id: &str) -> Option<&SourceDocument> {
        self.source_docs.get(doc_id).map(AsRef::as_ref)
    }

    /// Pipeline options this store was built with.
    pub fn options(&self) -> &PipelineOptions {
        &self.options
    }

    pub fn records(&self) -> Vec<&DocRecord> {
        let mut records: Vec<&DocRecord> = self.records.values().map(AsRef::as_ref).collect();
        records.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        records
    }

    pub fn record_by_id(&self, doc_id: &str) -> Option<&DocRecord> {
        self.records.get(doc_id).map(AsRef::as_ref)
    }

    /// Test-only: wipes a stored record's content hash to simulate a legacy
    /// persisted record (written before hashing existed).
    #[cfg(test)]
    pub(crate) fn clear_record_content_hash_for_test(&mut self, doc_id: &str) {
        if let Some(record) = Arc::make_mut(&mut self.records).get_mut(doc_id) {
            Arc::make_mut(record).content_hash.clear();
        }
    }

    pub fn memory_index_snapshot(&self) -> Option<&MemoryIndexSnapshot> {
        self.snapshot.as_deref()
    }

    pub fn inspection(&self) -> IndexStoreInspection {
        IndexStoreInspection {
            source_document_count: self.source_docs.len(),
            record_count: self.records.len(),
            dirty: self.dirty,
            store_revision: self.store_revision,
            snapshot_revision: self.snapshot_revision,
            tombstones: self.tombstones().into_iter().map(str::to_string).collect(),
            snapshot: self.snapshot.as_deref().map(inspect_memory_index_snapshot),
        }
    }

    pub fn chunk_lifecycle(&self) -> Vec<&ChunkLifecycleMeta> {
        let mut entries: Vec<&ChunkLifecycleMeta> = self.chunk_lifecycle.values().collect();
        entries.sort_by(|a, b| a.chunk_id.cmp(&b.chunk_id));
        entries
    }

    pub(crate) fn document_lifecycle(&self) -> Vec<DocumentLifecycleMeta> {
        let mut entries = Vec::new();
        for record in self.records.values().map(AsRef::as_ref) {
            let mut version = 0u32;
            let mut updated_at_ms = 0u64;
            let mut latest_chunk_ids = Vec::new();
            let mut superseded_chunk_ids = Vec::new();
            for chunk in &record.section_chunks {
                if let Some(meta) = self.chunk_lifecycle.get(&chunk.chunk_id) {
                    version = version.max(meta.version);
                    updated_at_ms = updated_at_ms.max(meta.updated_at_ms);
                    if meta.is_latest {
                        latest_chunk_ids.push(meta.chunk_id.clone());
                    } else {
                        superseded_chunk_ids.push(meta.chunk_id.clone());
                    }
                }
            }
            latest_chunk_ids.sort();
            superseded_chunk_ids.sort();
            let chunk_count = record.section_chunks.len();
            entries.push(DocumentLifecycleMeta {
                doc_id: record.doc_id.clone(),
                version: version.max(1),
                is_latest: chunk_count > 0 && superseded_chunk_ids.is_empty(),
                updated_at_ms,
                chunk_count,
                latest_chunk_ids,
                superseded_chunk_ids,
            });
        }
        entries.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        entries
    }

    pub fn temporal_facts(&self) -> Vec<&crate::temporal_fact::TemporalFact> {
        self.temporal_facts.facts().iter().collect::<Vec<_>>()
    }

    pub fn semantic_claims(&self) -> &[crate::semantic_relations::SemanticClaim] {
        self.semantic_relations.claims()
    }

    pub fn semantic_claim_states(
        &self,
        doc_id: &str,
    ) -> Vec<crate::semantic_relations::SemanticClaimState> {
        self.semantic_relations.claim_states_for_document(doc_id)
    }

    pub fn semantic_relations(&self) -> &[crate::semantic_relations::SemanticRelation] {
        self.semantic_relations.relations()
    }

    pub fn semantic_document_state(
        &self,
        doc_id: &str,
    ) -> crate::semantic_relations::DocumentSemanticState {
        self.semantic_relations.document_state(doc_id)
    }

    pub fn temporal_facts_as_of(&self, date: &str) -> Vec<&crate::temporal_fact::TemporalFact> {
        self.temporal_facts.as_of(date)
    }

    pub fn temporal_facts_as_known_at(
        &self,
        valid_date: &str,
        recorded_at: &str,
    ) -> Vec<crate::temporal_fact::TemporalFact> {
        self.temporal_facts.as_known_at(valid_date, recorded_at)
    }

    pub fn temporal_timeline(&self, subject: &str) -> Vec<&crate::temporal_fact::TemporalFact> {
        self.temporal_facts.timeline(subject)
    }

    pub fn temporal_timeline_window_around(
        &self,
        anchor: &str,
        before: usize,
        after: usize,
    ) -> Vec<crate::temporal_fact::TimelineEvent<'_>> {
        self.temporal_facts
            .timeline_window_around(anchor, before, after)
    }

    pub fn temporal_events_between(
        &self,
        start: &str,
        end: &str,
    ) -> Vec<crate::temporal_fact::TimelineEvent<'_>> {
        self.temporal_facts.timeline_events_between(start, end)
    }

    pub fn temporal_adjacent_pairs_between(
        &self,
        start: &str,
        end: &str,
        max_gap_days: Option<i64>,
    ) -> Vec<crate::temporal_fact::TimelinePair<'_>> {
        self.temporal_facts
            .adjacent_pairs_between(start, end, max_gap_days)
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.refresh_internal(true)
    }

    pub(crate) fn refresh_without_checkpoint(&mut self) -> Result<()> {
        self.refresh_internal(false)
    }

    fn refresh_internal(&mut self, checkpoint: bool) -> Result<()> {
        if self.dirty || self.snapshot.is_none() {
            self.prepare_pending_changes()?;
            self.pending_snapshot_changes
                .extend(self.tombstones.iter().cloned());
            self.build_prepared_snapshot()?;
        }
        if checkpoint && self.checkpoint_pending {
            persist_store_metadata(&self.store_paths, &self.options)?;
            self.persist_compatibility_state()?;
            self.checkpoint_pending = false;
        }
        Ok(())
    }

    /// Directory holding this store's per-segment lexical indexes, if it has one.
    pub(crate) fn segment_lexical_root(&self) -> Option<PathBuf> {
        self.store_paths
            .root
            .as_deref()
            .map(|root| root.join("segment-lexical"))
    }

    pub(crate) fn build_prepared_snapshot(&mut self) -> Result<()> {
        let previous = self.snapshot.clone();
        let segment_lexical_root = self.segment_lexical_root();
        self.build_prepared_snapshot_in(segment_lexical_root.as_deref())?;
        retire_replaced_lexical_dirs(previous.as_deref(), self.snapshot.as_deref());
        Ok(())
    }

    /// Build a frozen input whose records and semantic metadata were already
    /// prepared by the owner. Do not repeat corpus preparation in the worker.
    /// A detached build view has no store paths of its own, so the owner
    /// passes its `segment-lexical` directory: rebuilt segments then keep
    /// their lexical index on disk instead of in memory.
    pub(crate) fn build_prepared_snapshot_in(
        &mut self,
        segment_lexical_root: Option<&Path>,
    ) -> Result<()> {
        if self.dirty || self.snapshot.is_none() {
            let reprocessed_doc_ids = &self.pending_snapshot_changes;
            // The delta path uses previous membership and explicit changes;
            // only the full-rebuild fallback groups the entire corpus.
            let incremental = {
                let snapshot = &mut self.snapshot;
                let records = &self.records;
                let layout = &self.options.memory_index_layout;
                let store_revision = self.store_revision;
                Self::try_refresh_incremental(
                    snapshot,
                    records,
                    &reprocessed_doc_ids,
                    store_revision,
                    layout,
                    segment_lexical_root,
                )?
            };
            if !incremental {
                if matches!(
                    self.options.memory_index_layout,
                    MemoryIndexLayout::Segmented { .. }
                        | MemoryIndexLayout::AdaptiveSegmented { .. }
                ) {
                    let mut records = self.records.values().cloned().collect::<Vec<_>>();
                    records.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
                    self.snapshot = Some(Arc::new(MemoryIndexSnapshot::Segmented(
                        SegmentedMemoryIndex::from_shared_records_by_group_id_with_lexical_root(
                            &records,
                            self.store_revision,
                            segment_lexical_root,
                        ),
                    )));
                } else {
                    let mut records = self
                        .records
                        .values()
                        .map(AsRef::as_ref)
                        .cloned()
                        .collect::<Vec<DocRecord>>();
                    records.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
                    let global_index = Some(self.build_compatibility_index());
                    self.snapshot = Some(Arc::new(build_memory_index_snapshot(
                        records,
                        global_index,
                        &self.options.memory_index_layout,
                        self.store_revision,
                        segment_lexical_root,
                    )));
                }
            }
            self.snapshot_revision = self.store_revision;
            self.pending_snapshot_changes.clear();
            self.checkpoint_pending = true;
            self.dirty = false;
        }
        Ok(())
    }

    /// Rebuild only the segments touched by staged changes, sharing the rest
    /// with the previous snapshot. Returns `Ok(true)` when the incremental
    /// path ran; `Ok(false)` means the caller must do a full rebuild (first
    /// build, or a non-segmented layout). Sharing is structural — segments
    /// hold their index behind `Arc` — so there is no ownership dance and no
    /// silent fallback: a concurrent reader simply keeps the old snapshot.
    ///
    /// Takes the record map by reference. Changed IDs identify the affected
    /// segments; only rebuilt segments materialize owned records.
    fn try_refresh_incremental(
        snapshot: &mut Option<Arc<MemoryIndexSnapshot>>,
        records: &HashMap<String, Arc<DocRecord>>,
        reprocessed_doc_ids: &HashSet<String>,
        store_revision: u64,
        layout: &MemoryIndexLayout,
        segment_lexical_root: Option<&Path>,
    ) -> Result<bool> {
        if !matches!(
            layout,
            MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. }
        ) {
            return Ok(false);
        }
        let previous = match snapshot.as_deref() {
            Some(MemoryIndexSnapshot::Segmented(previous)) => previous,
            _ => return Ok(false),
        };
        let next = SegmentedMemoryIndex::refresh_changed_with_lexical_root(
            previous,
            records,
            reprocessed_doc_ids,
            store_revision,
            segment_lexical_root,
        )
        .map_err(|error| anyhow::anyhow!("incremental segment refresh failed: {error}"))?;
        *snapshot = Some(Arc::new(MemoryIndexSnapshot::Segmented(next)));
        Ok(true)
    }

    pub fn query(&mut self, query: &str, top_k: usize) -> Result<Vec<SearchResult>> {
        let prepared = PreparedQuery::new(query);
        self.query_prepared(&prepared, top_k, &std::collections::BTreeMap::new())
    }

    pub fn query_timed(
        &mut self,
        query: &str,
        top_k: usize,
    ) -> Result<(Vec<SearchResult>, QueryTimings, QueryDiagnostics)> {
        let prepared = PreparedQuery::new(query);
        self.query_prepared_timed(&prepared, top_k, &std::collections::BTreeMap::new())
    }

    /// Searches with the canonical query treatment: one [`PreparedQuery`] owns
    /// intent inference, query augmentation, temporal context, and the optional
    /// reference clock. `IndexStore` adds only semantic/filter scoping and
    /// snapshot layout routing.
    pub fn query_prepared(
        &mut self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<SearchResult>> {
        self.refresh()?;
        self.execute_prepared_on_snapshot(prepared, top_k, filters)
            .map(|(results, _, _)| results)
    }

    pub fn query_prepared_timed(
        &mut self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<(Vec<SearchResult>, QueryTimings, QueryDiagnostics)> {
        let refresh_start = std::time::Instant::now();
        self.refresh()?;
        let refresh_ms = refresh_start.elapsed().as_secs_f64() * 1000.0;
        let (results, mut timings, diagnostics) =
            self.execute_prepared_on_snapshot(prepared, top_k, filters)?;
        timings.refresh_ms += refresh_ms;
        timings.total_ms += refresh_ms;
        Ok((results, timings, diagnostics))
    }

    /// Query the current immutable snapshot without attempting a refresh.
    /// Callers may use this from concurrent read paths after writes have
    /// refreshed the store. An empty store has no snapshot and returns no hits.
    pub fn query_prepared_cached(
        &self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<SearchResult>> {
        if self.snapshot.is_none() {
            return Ok(Vec::new());
        }
        self.execute_prepared_on_snapshot(prepared, top_k, filters)
            .map(|(results, _, _)| results)
    }

    pub(crate) fn query_prepared_cached_with_allowed_doc_ids(
        &self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
        allowed_doc_ids: &HashSet<String>,
    ) -> Result<Vec<SearchResult>> {
        if self.snapshot.is_none() {
            return Ok(Vec::new());
        }
        execute_prepared_on_snapshot_parts(
            &self.options,
            &self.source_docs,
            &self.records,
            &self.semantic_relations,
            self.snapshot.as_deref(),
            prepared,
            top_k,
            filters,
            Some(allowed_doc_ids),
        )
        .map(|(results, _, _)| results)
    }

    /// Query the current immutable snapshot and retain internal diagnostics.
    /// This is intended for the single-question investigation harness.
    pub(crate) fn query_prepared_cached_with_diagnostics(
        &self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<(Vec<SearchResult>, QueryTimings, QueryDiagnostics)> {
        if self.snapshot.is_none() {
            return Ok((
                Vec::new(),
                QueryTimings::default(),
                QueryDiagnostics::default(),
            ));
        }
        self.execute_prepared_on_snapshot(prepared, top_k, filters)
    }

    pub fn query_filtered(
        &mut self,
        query: &str,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<SearchResult>> {
        let prepared = PreparedQuery::new(query);
        self.query_prepared(&prepared, top_k, filters)
    }

    fn execute_prepared_on_snapshot(
        &self,
        prepared: &PreparedQuery,
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<(Vec<SearchResult>, QueryTimings, QueryDiagnostics)> {
        execute_prepared_on_snapshot_parts(
            &self.options,
            &self.source_docs,
            &self.records,
            &self.semantic_relations,
            self.snapshot.as_deref(),
            prepared,
            top_k,
            filters,
            None,
        )
    }

    /// Multi-query convenience wrapper over the same prepared-query executor.
    /// Refresh happens once, then each query uses the current immutable snapshot.
    pub fn query_filtered_multi(
        &mut self,
        queries: &[&str],
        top_k: usize,
        filters: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<Vec<SearchResult>>> {
        self.refresh()?;
        queries
            .iter()
            .map(|query| {
                let prepared = PreparedQuery::new(query);
                self.execute_prepared_on_snapshot(&prepared, top_k, filters)
                    .map(|(results, _, _)| results)
            })
            .collect()
    }

    /// Stages pending source changes into records and derived state.
    ///
    /// Returns the ids of the documents whose records were actually rebuilt.
    /// A dirty document re-upserted with byte-identical content reuses its
    /// stored record (gated by `content_hash`): its segments, lexical
    /// entries, semantic state, and chunk lifecycle are already correct, so
    /// the rebuild is skipped entirely. Skipped docs are still cleared from
    /// `dirty_docs`.
    fn prepare_pending_changes(&mut self) -> Result<HashSet<String>> {
        let dirty_doc_ids = self.dirty_docs.iter().cloned().collect::<Vec<String>>();
        // Pass 1: content-hash gate. Unchanged docs reuse their stored record
        // and never reach the NLP below.
        let mut rebuild_ids: Vec<String> = Vec::new();
        for doc_id in &dirty_doc_ids {
            let incoming_hash = {
                let source_doc = self
                    .source_docs
                    .get(doc_id)
                    .expect("dirty doc should still exist in source docs");
                doc_record_content_hash(source_doc, &self.options)
            };
            let stored_hash = self
                .records
                .get(doc_id)
                .map(|record| record.content_hash.clone())
                .unwrap_or_default();
            if !stored_hash.is_empty() && stored_hash == incoming_hash {
                self.dirty_docs.remove(doc_id);
                continue;
            }
            rebuild_ids.push(doc_id.clone());
        }
        // Pass 2: batch the expensive NER daemon call across every doc being
        // rebuilt — one spaCy request, not one per document. Luyi 2026-09-29:
        // the refresh loop's per-doc NER was the bulk-build slowness; the
        // mutable side batches it before the immutable snapshot is published.
        let rebuild_docs: Vec<SourceDocument> = rebuild_ids
            .iter()
            .map(|doc_id| {
                self.source_docs
                    .get(doc_id)
                    .expect("dirty doc should still exist in source docs")
                    .as_ref()
                    .clone()
            })
            .collect();
        let tier1_inputs = source_documents_to_tier1_inputs(&rebuild_docs);
        let mut entities_by_doc = rank_key_entities_batched(&tier1_inputs, &self.options)?;
        // Pass 3: per-doc bookkeeping (semantic aggregate, chunk lifecycle)
        // with the precomputed NER entities.
        let mut rebuilt_records = Vec::with_capacity(rebuild_ids.len());
        for (doc_id, tier1_doc) in rebuild_ids.iter().zip(tier1_inputs.iter()) {
            let source_doc = self
                .source_docs
                .get(doc_id)
                .expect("dirty doc should still exist in source docs");
            let key_entities = entities_by_doc.remove(doc_id).unwrap_or_default();
            let record =
                build_doc_record_with_entities(source_doc, tier1_doc, key_entities, &self.options)?;
            rebuilt_records.push(record);
        }
        attach_bekind_evidence_claims(&mut rebuilt_records);

        let mut reprocessed_doc_ids = HashSet::new();
        let mut temporal_requires_rebuild = !self.tombstones.is_empty();
        for (doc_id, record) in rebuild_ids.iter().zip(rebuilt_records) {
            if matches!(self.options.memory_index_layout, MemoryIndexLayout::Single) {
                self.semantic_aggregate.remove_doc(doc_id);
                let semantic_state =
                    build_semantic_doc_state(&record, self.options.claim_extraction);
                self.semantic_aggregate.insert_doc_state(&semantic_state);
                self.semantic_docs.insert(doc_id.clone(), semantic_state);
            }
            let previous = self.records.get(doc_id).cloned();
            temporal_requires_rebuild |= previous.is_some();
            Arc::make_mut(&mut self.records).insert(doc_id.clone(), Arc::new(record));
            if let Some(current) = self.records.get(doc_id).cloned() {
                self.update_chunk_lifecycle_for_doc(doc_id, previous.as_deref(), &current);
            }
            self.dirty_docs.remove(doc_id);
            reprocessed_doc_ids.insert(doc_id.clone());
            self.pending_snapshot_changes.insert(doc_id.clone());
        }
        let tombstoned = self.tombstones.iter().cloned().collect::<Vec<_>>();
        for doc_id in tombstoned {
            self.remove_chunk_lifecycle_for_doc(&doc_id);
        }
        if !reprocessed_doc_ids.is_empty() || !self.tombstones.is_empty() {
            let appended = if temporal_requires_rebuild {
                false
            } else {
                let added_records = reprocessed_doc_ids
                    .iter()
                    .filter_map(|doc_id| self.records.get(doc_id).map(AsRef::as_ref))
                    .collect::<Vec<_>>();
                Arc::make_mut(&mut self.temporal_facts)
                    .append_records_if_ordered(added_records, &self.chunk_lifecycle)
            };
            if !appended {
                self.temporal_facts = Arc::new(TemporalFactStore::from_records(
                    self.records.values().map(AsRef::as_ref),
                    &self.chunk_lifecycle,
                ));
            }
        }
        // Incremental semantic update: only reprocessed documents get fresh
        // claims and only their canonical-claim neighborhoods get rebuilt
        // relations. Tombstoned documents drop their claims. Untouched
        // documents keep their existing claims, chains, and relations, which
        // is identical to a full rebuild (differential-tested in
        // semantic_relations::incremental_update_tests).
        let changed_docs = reprocessed_doc_ids
            .iter()
            .filter_map(|doc_id| self.source_docs.get(doc_id).map(AsRef::as_ref))
            .collect::<Vec<_>>();
        let removed_docs = self.tombstones.iter().cloned().collect::<Vec<_>>();
        Arc::make_mut(&mut self.semantic_relations).update_documents_borrowed(
            &self.source_docs,
            &changed_docs,
            &removed_docs,
            self.options.supersession,
        )?;
        Ok(reprocessed_doc_ids)
    }

    fn update_chunk_lifecycle_for_doc(
        &mut self,
        doc_id: &str,
        previous_record: Option<&DocRecord>,
        current_record: &DocRecord,
    ) {
        let now = current_time_ms();
        let mut current_lineage_keys = HashSet::new();
        for chunk in &current_record.section_chunks {
            let lineage_key = chunk_lineage_key(doc_id, chunk);
            current_lineage_keys.insert(lineage_key.clone());

            if let Some(existing) =
                Arc::make_mut(&mut self.chunk_lifecycle).get_mut(&chunk.chunk_id)
            {
                existing.doc_id = doc_id.to_string();
                existing.lineage_key = lineage_key.clone();
                existing.is_latest = true;
                existing.updated_at_ms = now;
                self.chunk_latest_by_lineage
                    .insert(lineage_key.clone(), chunk.chunk_id.clone());
                continue;
            }

            let previous_latest_chunk_id = self.chunk_latest_by_lineage.get(&lineage_key).cloned();
            let (version, supersedes_chunk_id) =
                if let Some(prev_chunk_id) = previous_latest_chunk_id {
                    if let Some(prev_meta) =
                        Arc::make_mut(&mut self.chunk_lifecycle).get_mut(&prev_chunk_id)
                    {
                        prev_meta.is_latest = false;
                        prev_meta.updated_at_ms = now;
                        (
                            prev_meta.version.saturating_add(1),
                            Some(prev_meta.chunk_id.clone()),
                        )
                    } else {
                        (1, None)
                    }
                } else {
                    (1, None)
                };

            Arc::make_mut(&mut self.chunk_lifecycle).insert(
                chunk.chunk_id.clone(),
                ChunkLifecycleMeta {
                    chunk_id: chunk.chunk_id.clone(),
                    doc_id: doc_id.to_string(),
                    lineage_key: lineage_key.clone(),
                    version,
                    is_latest: true,
                    supersedes_chunk_id,
                    updated_at_ms: now,
                    change_reason: Some("upsert".to_string()),
                },
            );
            self.chunk_latest_by_lineage
                .insert(lineage_key, chunk.chunk_id.clone());
        }

        if let Some(previous_record) = previous_record {
            for chunk in &previous_record.section_chunks {
                let lineage_key = chunk_lineage_key(doc_id, chunk);
                if !current_lineage_keys.contains(&lineage_key) {
                    if let Some(meta) =
                        Arc::make_mut(&mut self.chunk_lifecycle).get_mut(&chunk.chunk_id)
                    {
                        meta.is_latest = false;
                        meta.updated_at_ms = now;
                    }
                    self.chunk_latest_by_lineage.remove(&lineage_key);
                }
            }
        }
    }

    fn remove_chunk_lifecycle_for_doc(&mut self, doc_id: &str) {
        let chunk_ids = self
            .chunk_lifecycle
            .values()
            .filter(|meta| meta.doc_id == doc_id)
            .map(|meta| meta.chunk_id.clone())
            .collect::<Vec<_>>();
        for chunk_id in chunk_ids {
            if let Some(meta) = Arc::make_mut(&mut self.chunk_lifecycle).remove(&chunk_id) {
                if self
                    .chunk_latest_by_lineage
                    .get(&meta.lineage_key)
                    .map(|id| id == &meta.chunk_id)
                    .unwrap_or(false)
                {
                    self.chunk_latest_by_lineage.remove(&meta.lineage_key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn test_doc(doc_id: &str, group_id: Option<&str>, filters: &[(&str, &str)]) -> SourceDocument {
        SourceDocument {
            doc_id: doc_id.to_string(),
            source: format!("src:{doc_id}"),
            content: "test content".into(),
            concept: String::new(),
            group_id: group_id.map(str::to_string),
            headings: Vec::new(),
            links: Vec::new(),
            timestamp: None,
            doc_length: 12,
            author_agent: None,
            filters: filters
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        }
    }

    #[test]
    fn publication_composition_shares_unchanged_document_payloads() {
        let options = PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        };
        let mut owner = IndexStore::in_memory(options.clone());
        owner.upsert(test_doc("stable", Some("session"), &[]));
        owner.refresh().unwrap();
        let source_ptr = owner
            .source_document_by_id("stable")
            .unwrap()
            .content
            .as_ptr();
        let record_ptr = owner.records()[0].content.as_ptr();
        let composed = IndexStore::compose_segmented(
            IndexStore::in_memory(options),
            Some(owner.published_read_view()),
        )
        .unwrap();
        assert_eq!(
            source_ptr,
            composed
                .source_document_by_id("stable")
                .unwrap()
                .content
                .as_ptr(),
            "publication cloned source payload"
        );
        assert_eq!(
            record_ptr,
            composed.records()[0].content.as_ptr(),
            "publication cloned processed payload"
        );
        owner.upsert(test_doc("other", Some("other-session"), &[]));
        assert_eq!(
            source_ptr,
            owner
                .source_document_by_id("stable")
                .unwrap()
                .content
                .as_ptr(),
            "unrelated write cloned the corpus"
        );
        assert!(composed.source_document_by_id("other").is_none());
        let mut replacement = test_doc("stable", Some("session"), &[]);
        replacement.content = "updated content".into();
        owner.upsert(replacement);
        owner.refresh().unwrap();
        assert_eq!(
            composed.source_document_by_id("stable").unwrap().content,
            "test content"
        );
        assert_eq!(
            composed.record_by_id("stable").unwrap().content,
            "test content"
        );
        assert_eq!(
            owner.source_document_by_id("stable").unwrap().content,
            "updated content"
        );
        let retired_source = Arc::downgrade(&composed.source_docs["stable"]);
        let retired_record = Arc::downgrade(&composed.records["stable"]);
        let retired = Arc::downgrade(&composed.records);
        drop(composed);
        assert!(retired_source.upgrade().is_none());
        assert!(retired_record.upgrade().is_none());
        assert!(retired.upgrade().is_none(), "retired view remains retained");
    }

    /// Allocation regression: publishing must share immutable fact/claim storage.
    /// The mutation assertions also exercise snapshot isolation behavior.
    #[test]
    fn published_views_do_not_duplicate_fact_and_relation_allocations() {
        let mut owner = IndexStore::in_memory(PipelineOptions::default());
        for doc in correction_docs() {
            owner.upsert(doc);
        }
        owner.refresh().unwrap();
        assert!(!owner.semantic_claims().is_empty());
        owner.temporal_facts = serde_json::from_value::<TemporalFactStore>(serde_json::json!({
            "facts": [{"fact_id":"fact", "subject":"database", "predicate":"uses",
                "source_doc_id":"new", "source_chunk_id":"chunk", "source_chunk_version":1,
                "confidence":1.0, "is_latest":true}]
        }))
        .unwrap()
        .into();
        let view = owner.published_read_view();
        assert_eq!(
            owner.semantic_claims().as_ptr(),
            view.semantic_claims().as_ptr(),
            "publishing deep-copied semantic claims"
        );
        let facts = owner.temporal_facts();
        let published_facts = view.temporal_facts();
        assert!(!facts.is_empty(), "fixture must contain temporal facts");
        assert!(
            std::ptr::eq(facts[0], published_facts[0]),
            "publishing deep-copied temporal facts"
        );
        let claims_before = view.semantic_claims().to_vec();
        let facts_before = view
            .temporal_facts()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        owner.remove("new");
        owner.refresh().unwrap();
        assert_eq!(view.semantic_claims(), claims_before);
        assert_eq!(
            view.temporal_facts()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            facts_before
        );
        assert!(owner
            .semantic_claims()
            .iter()
            .all(|claim| claim.source_doc_id != "new"));
    }

    /// Allocation regression for redundant global postings. Retrieval and reopen
    /// assertions cover behavior independently of the storage representation.
    #[test]
    fn segmented_owner_does_not_retain_unused_global_postings() {
        let root = relation_test_root("no-global-postings");
        let options = PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        };
        let mut owner = IndexStore::at_path(&root, options.clone()).unwrap();
        for doc in correction_docs() {
            owner.upsert(doc);
        }
        owner.refresh().unwrap();
        assert!(
            owner.semantic_docs.is_empty(),
            "segmented owner retains duplicate per-document postings"
        );
        assert!(
            owner.semantic_aggregate.doc_to_chunks.is_empty(),
            "segmented owner retains unused global postings"
        );
        assert!(!owner
            .query_prepared_cached(&PreparedQuery::new("MongoDB"), 5, &BTreeMap::new())
            .unwrap()
            .is_empty());
        // Export still builds its compatibility index on demand.
        let _dump = owner.dump().unwrap();
        assert!(owner.semantic_docs.is_empty());
        drop(owner);
        let mut owner = IndexStore::at_path(&root, options).unwrap();
        assert!(
            owner.semantic_docs.is_empty(),
            "reopen rebuilt unused postings"
        );
        assert!(owner.semantic_aggregate.doc_to_chunks.is_empty());
        owner.remove("new");
        owner.refresh().unwrap();
        assert!(owner.semantic_docs.is_empty());
        assert!(owner.semantic_aggregate.doc_to_chunks.is_empty());
        assert!(owner.record_by_id("new").is_none());
        assert!(owner.record_by_id("old").is_some());
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn segmented_indexes_reuse_owner_record_allocations() {
        let mut owner = IndexStore::in_memory(PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        });
        for doc in correction_docs() {
            owner.upsert(doc);
        }
        owner.refresh().unwrap();

        let Some(MemoryIndexSnapshot::Segmented(index)) = owner.snapshot.as_deref() else {
            panic!("expected segmented snapshot");
        };
        for (doc_id, owner_record) in owner.records.iter() {
            let segment_record = index
                .segments
                .iter()
                .find_map(|segment| segment.index.docs.get(doc_id))
                .unwrap_or_else(|| panic!("segment is missing {doc_id}"));
            let segment_record: &DocRecord = segment_record;
            assert!(
                std::ptr::eq(owner_record.as_ref(), segment_record),
                "segmented index cloned the full record for {doc_id}"
            );
        }
    }

    fn relation_test_root(name: &str) -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "membrick-relations-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn correction_docs() -> Vec<SourceDocument> {
        [
            (
                "old",
                "We use Postgres for analytics.",
                "2023-11-14T00:00:00Z",
            ),
            (
                "new",
                "We use MongoDB for analytics instead of Postgres.",
                "2023-11-15T00:00:00Z",
            ),
        ]
        .into_iter()
        .map(|(doc_id, content, timestamp)| {
            let mut doc = test_doc(doc_id, Some("session"), &[]);
            doc.content = content.into();
            doc.doc_length = content.len();
            doc.timestamp = Some(timestamp.into());
            doc
        })
        .collect()
    }

    /// Relations a restart computes: a full rebuild from reconstructed records.
    fn assert_restart_equivalent(relations: &SemanticRelationStore, store: &IndexStore) {
        let reconstructed = store
            .records
            .values()
            .map(|record| source_document_from_record(record))
            .collect::<Vec<_>>();
        let expected = SemanticRelationStore::try_from_documents(
            reconstructed.iter(),
            store.options.supersession,
        )
        .unwrap();
        assert_eq!(relations.claims(), expected.claims());
        assert_eq!(relations.relations(), expected.relations());
        for doc_id in store.records.keys() {
            assert_eq!(
                relations.document_state(doc_id),
                expected.document_state(doc_id),
                "{doc_id}"
            );
        }
    }

    #[test]
    fn reopened_store_loads_relations_saved_at_checkpoint() {
        let root = relation_test_root("reopen");
        let mut store = IndexStore::at_path(&root, PipelineOptions::default()).unwrap();
        for doc in correction_docs() {
            store.upsert(doc);
        }
        store.refresh().unwrap();
        assert!(!store.semantic_relations().is_empty());
        drop(store);
        assert!(root.join("semantic").join(SEMANTIC_RELATIONS_FILE).exists());

        let reopened = IndexStore::at_path(&root, PipelineOptions::default()).unwrap();
        let cached = super::super::load_semantic_relations(
            &reopened.store_paths,
            &reopened.records,
            reopened.options.supersession,
        )
        .expect("the checkpoint cache matches the reopened records");
        assert_restart_equivalent(&cached, &reopened);
        assert_restart_equivalent(&reopened.semantic_relations, &reopened);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn segmented_store_saves_its_catalog_cache_after_each_write() {
        let root = relation_test_root("catalog");
        let options = PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        };
        let cache = crate::segments::segment_catalog_cache_path(&root);
        let mut store = IndexStore::at_path(&root, options.clone()).unwrap();
        for doc in correction_docs() {
            store.upsert(doc);
        }
        store.refresh().unwrap();
        let first = std::fs::read(&cache).expect("refresh writes the catalog cache");

        let mut added = test_doc("later", Some("other-session"), &[]);
        added.content = "We deploy with Nomad.".into();
        added.doc_length = added.content.len();
        store.upsert(added);
        store.refresh().unwrap();
        let second = std::fs::read(&cache).unwrap();
        assert_ne!(first, second, "a write refreshes the saved catalog");
        drop(store);

        let reopened = IndexStore::at_path(&root, options).unwrap();
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            second,
            "an unchanged reopen reuses the saved catalog"
        );
        assert!(!reopened
            .query_prepared_cached(
                &crate::query_plan::PreparedQuery::new("Nomad"),
                5,
                &BTreeMap::new()
            )
            .unwrap()
            .is_empty());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_relations_match_a_restart_when_concept_is_not_persisted() {
        let root = relation_test_root("concept");
        let mut store = IndexStore::at_path(&root, PipelineOptions::default()).unwrap();
        for mut doc in correction_docs() {
            // Records do not keep `concept`; a restart derives a different one.
            doc.concept = format!("live-only-{}", doc.doc_id);
            store.upsert(doc);
        }
        store.refresh().unwrap();
        let cached = super::super::load_semantic_relations(
            &store.store_paths,
            &store.records,
            store.options.supersession,
        )
        .expect("checkpoint wrote a cache");
        assert_restart_equivalent(&cached, &store);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_records_invalidate_the_relation_cache() {
        let root = relation_test_root("invalidate");
        let mut store = IndexStore::at_path(&root, PipelineOptions::default()).unwrap();
        for doc in correction_docs() {
            store.upsert(doc);
        }
        store.refresh().unwrap();
        let cache = root.join("semantic").join(SEMANTIC_RELATIONS_FILE);
        let saved = std::fs::read(&cache).unwrap();

        let mut changed = correction_docs().remove(1);
        changed.content = "We use SQLite for analytics instead of Postgres.".into();
        changed.doc_length = changed.content.len();
        store.upsert(changed);
        store.refresh().unwrap();
        // Restore the cache written for the previous records.
        std::fs::write(&cache, saved).unwrap();
        assert!(super::super::load_semantic_relations(
            &store.store_paths,
            &store.records,
            store.options.supersession,
        )
        .is_none());
        let disabled = crate::semantic_relations::SupersessionOptions {
            enabled: false,
            ..store.options.supersession
        };
        assert!(super::super::load_semantic_relations(
            &store.store_paths,
            &store.records,
            disabled
        )
        .is_none());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn composed_relations_match_a_full_rebuild_of_the_union() {
        let options = PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        };
        let doc = |doc_id: &str, content: &str, timestamp: &str| {
            let mut doc = test_doc(doc_id, Some("session"), &[]);
            doc.content = content.into();
            doc.doc_length = content.len();
            doc.timestamp = Some(timestamp.into());
            doc
        };
        let mut workspace = IndexStore::in_memory(options.clone());
        workspace.upsert(doc(
            "old",
            "We use Postgres for analytics.",
            "2023-11-14T00:00:00Z",
        ));
        workspace.refresh().unwrap();
        let mut provider = IndexStore::in_memory(options);
        provider.upsert(doc(
            "new",
            "We use MongoDB for analytics instead of Postgres.",
            "2023-11-15T00:00:00Z",
        ));
        provider.upsert(doc(
            "other",
            "We use Redis for caching.",
            "2023-11-15T00:00:00Z",
        ));
        provider.refresh().unwrap();
        let union = [&workspace, &provider]
            .iter()
            .flat_map(|store| store.source_docs.values().cloned())
            .collect::<Vec<_>>();
        let expected = SemanticRelationStore::try_from_documents(
            union.iter().map(AsRef::as_ref),
            workspace.options.supersession,
        )
        .unwrap();
        assert!(
            !expected.relations().is_empty(),
            "the fixture must produce a cross-store relation"
        );

        let view = IndexStore::compose_segmented_read_view(
            workspace.published_read_view(),
            Some(provider.published_read_view()),
        )
        .unwrap();
        assert_eq!(view.semantic_relations(), expected.relations());
        assert_eq!(view.semantic_claims(), expected.claims());
        for doc_id in ["old", "new", "other"] {
            assert_eq!(
                view.semantic_document_state(doc_id),
                expected.document_state(doc_id),
                "{doc_id}"
            );
        }
    }

    #[test]
    fn published_composition_omits_writer_only_aggregates() {
        let options = PipelineOptions {
            memory_index_layout: MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
            ..PipelineOptions::default()
        };
        let mut owner = IndexStore::in_memory(options.clone());
        owner.upsert(test_doc("stable", Some("session"), &[]));
        owner.refresh().unwrap();
        let view = IndexStore::compose_segmented_read_view(
            IndexStore::in_memory(options),
            Some(owner.published_read_view()),
        )
        .unwrap();
        assert!(view.semantic_docs.is_empty());
        assert!(view.semantic_aggregate.doc_to_chunks.is_empty());
        assert!(view.chunk_latest_by_lineage.is_empty());
        assert!(!view
            .query_prepared_cached(
                &crate::query_plan::PreparedQuery::new("test content"),
                5,
                &BTreeMap::new()
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn borrowed_checkpoint_preserves_the_existing_record_format() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "membrick-checkpoint-format-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut owner = IndexStore::at_path(&root, PipelineOptions::default()).unwrap();
        let mut doc = test_doc("stable", Some("session"), &[("provider", "codex")]);
        doc.content = "Violet lantern deployment. Preserve café and 中文 evidence.".into();
        doc.timestamp = Some("2026-10-17T22:00:00Z".into());
        doc.headings = vec!["Deployment decision".into()];
        doc.links = vec!["original-observation".into()];
        owner.upsert(doc);
        owner.refresh().unwrap();
        let actual = serde_json::json!({
            "records": super::super::persisted_record_values(&root.join("semantic")),
        });
        let expected: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&PersistedDocRecord::from(
                owner.record_by_id("stable").unwrap().clone(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(actual["records"][0], expected);
        assert_eq!(
            actual["records"][0]["content"],
            owner
                .source_document_by_id("stable")
                .unwrap()
                .content
                .as_str()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn docs_map(docs: Vec<SourceDocument>) -> HashMap<String, Arc<SourceDocument>> {
        docs.into_iter()
            .map(|d| (d.doc_id.clone(), Arc::new(d)))
            .collect()
    }

    #[test]
    fn group_members_honor_filters() {
        let docs = docs_map(vec![
            test_doc("d1", Some("g1"), &[("user", "alice")]),
            test_doc("d2", Some("g1"), &[("user", "bob")]),
            test_doc("d3", Some("g1"), &[("user", "alice")]),
        ]);
        let filters: BTreeMap<String, String> = [("user".to_string(), "alice".to_string())]
            .into_iter()
            .collect();
        let mut ids = filtered_group_member_ids(&docs, "g1", &filters);
        ids.sort();
        assert_eq!(ids, vec!["d1".to_string(), "d3".to_string()]);
    }

    #[test]
    fn ungrouped_doc_resolves_as_singleton() {
        let docs = docs_map(vec![test_doc("solo", None, &[("user", "alice")])]);
        let filters: BTreeMap<String, String> = [("user".to_string(), "alice".to_string())]
            .into_iter()
            .collect();
        // Stage 1 keys ungrouped documents by doc id; the member lookup must
        // resolve that key back to the document instead of dropping it.
        let ids = filtered_group_member_ids(&docs, "solo", &filters);
        assert_eq!(ids, vec!["solo".to_string()]);
        // ...but not when the filters exclude it.
        let other: BTreeMap<String, String> = [("user".to_string(), "mallory".to_string())]
            .into_iter()
            .collect();
        assert!(filtered_group_member_ids(&docs, "solo", &other).is_empty());
    }

    #[test]
    fn unknown_group_yields_no_members() {
        let docs = docs_map(vec![test_doc("d1", Some("g1"), &[("user", "alice")])]);
        let filters: BTreeMap<String, String> = BTreeMap::new();
        assert!(filtered_group_member_ids(&docs, "nope", &filters).is_empty());
    }
}
