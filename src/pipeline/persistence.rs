use super::{
    ChunkLifecycleMeta, IndexLocation, MemoryIndexLayout, MemoryIndexSegmentInspection,
    MemoryIndexSnapshot, MemoryIndexSnapshotInspection, PersistedDocRecord,
    PersistedSemanticRecords, PipelineOptions, StoreMetadata, StorePaths, CHUNK_LIFECYCLE_FILE,
    SEGMENT_MANIFEST_FILE, SEMANTIC_CORE_FILE, SEMANTIC_RECORDS_FILE, SEMANTIC_RELATIONS_FILE,
    STORE_LAYOUT_VERSION, STORE_SCHEMA_VERSION,
};
use crate::index::{DocRecord, MemoryIndex, QueryDiagnostics, QueryTimings, SearchResult};
use crate::query_plan::PreparedQuery;
use crate::segments::SegmentManifest;
use crate::semantic_relations::{SemanticRelationStore, SupersessionOptions};
use crate::source::SourceDocument;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
pub(crate) fn execute_prepared_on_snapshot_parts(
    options: &PipelineOptions,
    source_docs: &HashMap<String, Arc<SourceDocument>>,
    records: &HashMap<String, Arc<DocRecord>>,
    semantic_relations: &SemanticRelationStore,
    snapshot: Option<&MemoryIndexSnapshot>,
    prepared: &PreparedQuery,
    top_k: usize,
    filters: &std::collections::BTreeMap<String, String>,
    additional_allowed_doc_ids: Option<&HashSet<String>>,
) -> Result<(Vec<SearchResult>, QueryTimings, QueryDiagnostics)> {
    let profile = std::env::var_os("MEMBRICK_QUERY_TIMINGS").is_some();
    let query_started = std::time::Instant::now();
    let Some(snapshot) = snapshot else {
        return Ok((
            Vec::new(),
            QueryTimings::default(),
            QueryDiagnostics::default(),
        ));
    };
    let filter_started = std::time::Instant::now();
    let needs_semantic_visibility =
        prepared.semantic_visibility_requires_doc_ids(semantic_relations);
    let filter_bitmap = if additional_allowed_doc_ids.is_some()
        || filters.is_empty()
        || (filters.len() == 1
            && needs_semantic_visibility
            && matches!(snapshot, MemoryIndexSnapshot::Single(_)))
    {
        None
    } else {
        match snapshot {
            MemoryIndexSnapshot::Single(index) => index.doc_bitmap_matching_filters(filters),
            MemoryIndexSnapshot::Segmented(_) => None,
        }
    };
    // For a one-filter, one-segment query, apply semantic supersession as a
    // bitmap subtraction. This avoids scanning every source document and
    // materializing an allow-list containing nearly the whole corpus.
    let effective_filter_bitmap = if additional_allowed_doc_ids.is_some() {
        None
    } else {
        match snapshot {
            MemoryIndexSnapshot::Single(index)
                if filters.len() == 1 && needs_semantic_visibility =>
            {
                let (key, value) = filters.first_key_value().expect("one filter");
                index
                    .doc_bitmap_for_single_filter(key, value)
                    .map(|posting| {
                        let mut allowed = posting.clone();
                        allowed -= index
                            .doc_bitmap_for_id_iter(semantic_relations.superseded_document_ids());
                        allowed
                    })
            }
            MemoryIndexSnapshot::Segmented(segmented)
                if filters.len() == 1
                    && segmented.segment_count() == 1
                    && needs_semantic_visibility =>
            {
                let (key, value) = filters.first_key_value().expect("one filter");
                let index = &segmented.segments[0].index;
                index
                    .doc_bitmap_for_single_filter(key, value)
                    .map(|posting| {
                        let mut allowed = posting.clone();
                        allowed -= index
                            .doc_bitmap_for_id_iter(semantic_relations.superseded_document_ids());
                        allowed
                    })
            }
            _ => None,
        }
    };
    let filter_allowed = if let Some(additional_allowed) = additional_allowed_doc_ids {
        Some(
            source_docs
                .iter()
                .filter(|(doc_id, document)| {
                    additional_allowed.contains(*doc_id)
                        && filters.iter().all(|(key, value)| {
                            document
                                .filters
                                .get(key)
                                .is_some_and(|actual| actual == value)
                        })
                })
                .map(|(doc_id, _)| doc_id.clone())
                .collect::<HashSet<_>>(),
        )
    } else if filters.is_empty() || effective_filter_bitmap.is_some() {
        None
    } else if let MemoryIndexSnapshot::Segmented(segmented) = snapshot {
        // Build the routing scope from the per-segment bitmap postings. This
        // preserves filter-aware segment selection without scanning records.
        // Per-segment filter bitmaps below are enough to prune routing and
        // constrain local queries. Materialize IDs only when semantic
        // supersession needs an explicit allow-list.
        if needs_semantic_visibility {
            segmented.doc_ids_matching_filters(filters)
        } else {
            None
        }
    } else if let MemoryIndexSnapshot::Single(index) = snapshot {
        // Only materialize IDs when semantic supersession must be intersected
        // with the filter. Without that policy, the query can enforce filters
        // directly from the index bitmap.
        if needs_semantic_visibility {
            index.doc_ids_matching_filters(filters)
        } else {
            None
        }
    } else {
        Some(
            records
                .iter()
                .filter(|(_, record)| {
                    filters.iter().all(|(key, value)| {
                        record
                            .filters
                            .get(key)
                            .is_some_and(|actual| actual == value)
                    })
                })
                .map(|(doc_id, _)| doc_id.clone())
                .collect::<HashSet<_>>(),
        )
    };
    // The common HTTP path scopes by one user filter. With a one-segment
    // snapshot, borrow that immutable posting directly instead of cloning the
    // bitmap into a temporary HashMap for every request.
    let single_segment_filter_bitmap = if additional_allowed_doc_ids.is_some() {
        None
    } else {
        match snapshot {
            MemoryIndexSnapshot::Segmented(segmented)
                if filters.len() == 1
                    && segmented.segment_count() == 1
                    && !needs_semantic_visibility =>
            {
                let (key, value) = filters.first_key_value().expect("one filter");
                segmented.segments[0]
                    .index
                    .doc_bitmap_for_single_filter(key, value)
            }
            _ => None,
        }
    };
    let filter_segment_bitmaps = if additional_allowed_doc_ids.is_some()
        || filters.is_empty()
        || single_segment_filter_bitmap.is_some()
        || effective_filter_bitmap.is_some()
    {
        None
    } else {
        match snapshot {
            MemoryIndexSnapshot::Segmented(segmented) => {
                Some(segmented.doc_bitmaps_matching_filters(filters))
            }
            MemoryIndexSnapshot::Single(_) => None,
        }
    };
    if profile {
        eprintln!(
            "query_timing filter_ms={:.3} filters={} segments={} semantics_empty={}",
            filter_started.elapsed().as_secs_f64() * 1000.0,
            filters.len(),
            match snapshot {
                MemoryIndexSnapshot::Single(_) => 1,
                MemoryIndexSnapshot::Segmented(segmented) => segmented.segment_count(),
            },
            semantic_relations.is_empty(),
        );
    }
    // With no semantic relations, semantic visibility is a no-op and the
    // document IDs are never read. Avoid cloning every ID for ordinary
    // lexical queries.
    let document_ids = if !needs_semantic_visibility || effective_filter_bitmap.is_some() {
        Vec::new()
    } else {
        source_docs.keys().cloned().collect::<Vec<_>>()
    };
    let allowed_doc_ids =
        prepared.semantic_allowed_doc_ids(filter_allowed, semantic_relations, &document_ids);

    let (results, timings, diagnostics) = match snapshot {
        MemoryIndexSnapshot::Segmented(segmented) => {
            let (query_top_n, max_query_n, routing_strategy, adaptive) =
                match &options.memory_index_layout {
                    MemoryIndexLayout::Segmented {
                        query_top_n,
                        routing_strategy,
                    } => (*query_top_n, *query_top_n, *routing_strategy, false),
                    MemoryIndexLayout::AdaptiveSegmented {
                        query_top_n,
                        max_query_n,
                        routing_strategy,
                    } => (*query_top_n, *max_query_n, *routing_strategy, true),
                    MemoryIndexLayout::Single => {
                        anyhow::bail!("segmented snapshot published under single-index options")
                    }
                };
            let mut context = prepared.temporal_context();
            context.freshness_policy = options.freshness_policy;
            context.allowed_doc_ids = allowed_doc_ids.as_ref();
            context.allowed_doc_bitmap = filter_bitmap.as_ref();
            context.allowed_segment_doc_bitmaps = filter_segment_bitmaps.as_ref();
            let started = std::time::Instant::now();
            let multi_segment = segmented.segment_count() > 1;
            let output = if !multi_segment {
                let local_bitmap = filter_segment_bitmaps
                    .as_ref()
                    .and_then(|maps| maps.values().next())
                    .or(effective_filter_bitmap.as_ref())
                    .or(single_segment_filter_bitmap);
                let mut local_context = context;
                if allowed_doc_ids.is_some() {
                    // The ID allow-list is materialized (field filters,
                    // possibly intersected with the semantic supersession
                    // allow-list): it is authoritative. Drop the bitmap --
                    // downstream prefers the bitmap over the ID set, and the
                    // bitmap cannot express supersession.
                    local_context.allowed_doc_bitmap = None;
                } else {
                    local_context.allowed_doc_ids = None;
                    local_context.allowed_doc_bitmap = local_bitmap;
                }
                let (results, query_timings) = segmented
                    .query_single_segment(
                        prepared.search_query(),
                        top_k,
                        local_context,
                        prepared.reference_date(),
                    )
                    .unwrap_or_default();
                if profile {
                    eprintln!(
                        "query_timing index_total_ms={:.3} lexical_ms={:.3} snapshot_ms={:.3} rerank_ms={:.3}",
                        query_timings.total_ms,
                        query_timings.lexical_bm25_ms,
                        query_timings.snapshot_query_ms,
                        query_timings.rerank_ms,
                    );
                }
                crate::segments::SegmentQueryOutput {
                    results,
                    diagnostics: Default::default(),
                }
            } else if adaptive {
                segmented
                    .query_with_adaptive_segment_enrichment_temporal_context_and_strategy(
                        prepared.search_query(),
                        top_k,
                        query_top_n.max(1),
                        max_query_n.max(query_top_n).max(1),
                        routing_strategy,
                        context,
                        prepared.reference_date(),
                    )
                    .0
            } else {
                segmented.query_with_temporal_context_at_and_diagnostics_and_strategy(
                    prepared.search_query(),
                    top_k,
                    query_top_n.max(1),
                    routing_strategy,
                    context,
                    prepared.reference_date(),
                )
            };
            // The corpus-wide ("global") arm is fused with the routed arm via
            // reciprocal rank fusion when `fuse_global_arm` is set: a routed
            // segment arm can miss when the router selects the wrong segments,
            // so its ranking is fused with an all-segments ranking. Label-free
            // (ranks only), so the two arms' score scales never crowd each
            // other out. With a strong router the global arm rescues little
            // and costs most of the query latency, so it can be disabled.
            let fuse_global = options.fuse_global_arm && multi_segment;
            let (results, global_diagnostics) = if fuse_global {
                let global_output = segmented
                    .query_all_segments_with_temporal_context_and_diagnostics(
                        prepared.search_query(),
                        top_k,
                        context,
                    );
                let fused = crate::segments::reciprocal_rank_fusion(
                    &[output.results.as_slice(), global_output.results.as_slice()],
                    top_k,
                );
                (fused, Some(global_output.diagnostics))
            } else {
                (output.results, None)
            };
            let timings = QueryTimings {
                total_ms: started.elapsed().as_secs_f64() * 1000.0,
                ..QueryTimings::default()
            };
            // Diagnostics account for both arms: the fused ranking is drawn
            // from the routed arm's candidates plus the global arm's.
            let global_merged = global_diagnostics
                .as_ref()
                .map(|d| d.merged_result_count)
                .unwrap_or(0);
            let global_queried = global_diagnostics
                .as_ref()
                .map(|d| d.queried_segment_count)
                .unwrap_or(0);
            if profile {
                eprintln!(
                    "query_timing segmented_ms={:.3} total_ms={:.3} segments={}",
                    timings.total_ms,
                    query_started.elapsed().as_secs_f64() * 1000.0,
                    output.diagnostics.queried_segment_count + global_queried
                );
            }
            let diagnostics = QueryDiagnostics {
                candidates: output.diagnostics.merged_result_count + global_merged,
                snapshot_generation: output.diagnostics.snapshot_generation,
                shard_completeness: output.diagnostics.shard_completeness,
                ..QueryDiagnostics::default()
            };
            (
                prepared.annotate_semantic_results(results, semantic_relations, top_k),
                timings,
                diagnostics,
            )
        }
        MemoryIndexSnapshot::Single(index) => {
            let mut context = prepared.temporal_context();
            context.freshness_policy = options.freshness_policy;
            context.allowed_doc_ids = allowed_doc_ids.as_ref();
            context.allowed_doc_bitmap = effective_filter_bitmap
                .as_ref()
                .or_else(|| filter_bitmap.as_ref().filter(|_| allowed_doc_ids.is_none()));
            let (results, timings, diagnostics) = index.query_with_temporal_context_at(
                prepared.search_query(),
                top_k,
                context,
                prepared.reference_date(),
            );
            (
                prepared.annotate_semantic_results(results, semantic_relations, top_k),
                timings,
                diagnostics,
            )
        }
    };

    Ok((results, timings, diagnostics))
}

pub(crate) fn inspect_memory_index_snapshot(
    snapshot: &MemoryIndexSnapshot,
) -> MemoryIndexSnapshotInspection {
    match snapshot {
        MemoryIndexSnapshot::Single(index) => {
            let mut doc_ids = index.docs.keys().cloned().collect::<Vec<_>>();
            doc_ids.sort();
            MemoryIndexSnapshotInspection {
                layout: "single".to_string(),
                segment_count: 1,
                global_document_count: index.docs.len(),
                segments: vec![MemoryIndexSegmentInspection {
                    segment_id: "global".to_string(),
                    document_count: doc_ids.len(),
                    doc_ids,
                    profile_term_count: 0,
                    profile_entity_count: 0,
                    profile_topic_count: 0,
                    profile_local_memory_count: 0,
                }],
            }
        }
        MemoryIndexSnapshot::Segmented(index) => MemoryIndexSnapshotInspection {
            layout: "segmented".to_string(),
            segment_count: index.segments.len(),
            global_document_count: index
                .segments
                .iter()
                .map(|segment| segment.doc_ids.len())
                .sum(),
            segments: index
                .segments
                .iter()
                .map(|segment| {
                    let (term_count, entity_count, topic_count, local_memory_count) =
                        index.routing_summary_counts(&segment.segment_id);
                    MemoryIndexSegmentInspection {
                        segment_id: segment.segment_id.clone(),
                        document_count: segment.doc_ids.len(),
                        doc_ids: segment.doc_ids.clone(),
                        profile_term_count: term_count,
                        profile_entity_count: entity_count,
                        profile_topic_count: topic_count,
                        profile_local_memory_count: local_memory_count,
                    }
                })
                .collect(),
        },
    }
}

fn index_location_name(index_location: &IndexLocation) -> String {
    match index_location {
        IndexLocation::InMemory => "in_memory".to_string(),
        IndexLocation::UnderCorpusRoot => "under_corpus_root".to_string(),
        IndexLocation::Explicit(_) => "explicit".to_string(),
    }
}

fn current_store_metadata(options: &PipelineOptions) -> StoreMetadata {
    StoreMetadata {
        schema_version: STORE_SCHEMA_VERSION,
        layout_version: STORE_LAYOUT_VERSION.to_string(),
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        index_location: index_location_name(&options.index_location),
    }
}

fn load_store_metadata(metadata_path: &Path) -> Result<Option<StoreMetadata>> {
    if !metadata_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(metadata_path)?;
    Ok(Some(serde_json::from_str(&contents)?))
}

pub(crate) fn persist_store_metadata(
    store_paths: &StorePaths,
    options: &PipelineOptions,
) -> Result<()> {
    let Some(metadata_path) = store_paths.metadata_path.as_ref() else {
        return Ok(());
    };
    let metadata = current_store_metadata(options);
    write_text_file_atomic(metadata_path, &serde_json::to_string_pretty(&metadata)?)?;
    Ok(())
}

pub(crate) fn ensure_store_metadata(
    store_paths: &StorePaths,
    options: &PipelineOptions,
) -> Result<()> {
    let Some(metadata_path) = store_paths.metadata_path.as_ref() else {
        return Ok(());
    };
    if let Some(root) = store_paths.root.as_ref() {
        fs::create_dir_all(root)?;
    }
    if let Some(existing) = load_store_metadata(metadata_path)? {
        let current = current_store_metadata(options);
        if existing.schema_version != current.schema_version {
            anyhow::bail!(
                "index schema mismatch at {}: found {}, expected {}",
                metadata_path.display(),
                existing.schema_version,
                current.schema_version
            );
        }
        if existing.layout_version != current.layout_version {
            anyhow::bail!(
                "index layout mismatch at {}: found {}, expected {}",
                metadata_path.display(),
                existing.layout_version,
                current.layout_version
            );
        }
        return Ok(());
    }
    persist_store_metadata(store_paths, options)
}

pub(crate) fn is_directory_empty(dir: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(dir)?;
    Ok(entries.next().is_none())
}

fn semantic_records_path(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(SEMANTIC_RECORDS_FILE))
}

fn semantic_core_path(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(SEMANTIC_CORE_FILE))
}

fn segment_manifest_path(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(SEGMENT_MANIFEST_FILE))
}

pub(crate) fn load_segment_manifest(store_paths: &StorePaths) -> Result<Option<SegmentManifest>> {
    let Some(path) = segment_manifest_path(store_paths) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
}

pub(crate) fn persist_segment_manifest(
    store_paths: &StorePaths,
    manifest: &SegmentManifest,
) -> Result<()> {
    let Some(semantic_dir) = store_paths.semantic_dir.as_ref() else {
        return Ok(());
    };
    fs::create_dir_all(semantic_dir)?;
    let path = segment_manifest_path(store_paths)
        .expect("segment manifest path should exist when semantic dir exists");
    write_text_file_atomic(&path, &serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

fn chunk_lifecycle_path(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(CHUNK_LIFECYCLE_FILE))
}

pub(crate) fn chunk_lineage_key(doc_id: &str, chunk: &crate::index::SectionChunk) -> String {
    format!(
        "{}::{}::{}::{}",
        doc_id,
        chunk.start_line,
        chunk.end_line,
        chunk.heading.trim().to_lowercase()
    )
}

pub(crate) fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Records do not store `concept`; a reopened store derives it this way.
fn reconstructed_concept(record: &DocRecord) -> &str {
    record
        .headings
        .first()
        .or(record.probable_topic.as_ref())
        .unwrap_or(&record.doc_id)
}

pub(crate) fn source_document_from_record(record: &DocRecord) -> SourceDocument {
    SourceDocument {
        doc_id: record.doc_id.clone(),
        source: record.source.clone(),
        // A reopened store keeps one copy: the document shares the record's text.
        content: record.content.clone(),
        concept: reconstructed_concept(record).to_string(),
        group_id: record.group_id.clone(),
        filters: record.filters.clone(),
        headings: record.headings.clone(),
        links: record.doc_links.clone(),
        timestamp: record.timestamp.clone(),
        doc_length: record.doc_length,
        author_agent: record.author_agent.clone(),
        // Restore the persisted phrases and their extraction stamp so a
        // reopened index knows which documents are already done (an empty
        // phrase list with a matching stamp is a legitimate empty result,
        // not a document still waiting for extraction).
        key_phrases: record.key_phrases.clone(),
        key_phrase_extraction_hash: record.key_phrase_extraction_hash.clone(),
    }
}

/// Whether `doc` equals what [`source_document_from_record`] rebuilds from
/// `record`, for every field relation extraction reads. Keep this in step with
/// that function. Key phrases are restored too but never read by relations.
fn matches_reconstruction(doc: &SourceDocument, record: &DocRecord) -> bool {
    doc.doc_id == record.doc_id
        && doc.source == record.source
        && record.content == doc.content
        && doc.concept == reconstructed_concept(record)
        && doc.group_id == record.group_id
        && doc.filters == record.filters
        && doc.headings == record.headings
        && doc.links == record.doc_links
        && doc.timestamp == record.timestamp
        && doc.doc_length == record.doc_length
        && doc.author_agent == record.author_agent
}

/// Bump when claim extraction or relation detection changes, so caches
/// written by older code are rebuilt instead of reused.
const SEMANTIC_RELATIONS_CACHE_VERSION: u32 = 1;

#[derive(serde::Deserialize)]
struct PersistedSemanticRelations {
    version: u32,
    fingerprint: String,
    store: SemanticRelationStore,
}

#[derive(serde::Serialize)]
struct PersistedSemanticRelationsRef<'a> {
    version: u32,
    fingerprint: &'a str,
    store: &'a SemanticRelationStore,
}

fn semantic_relations_path(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(SEMANTIC_RELATIONS_FILE))
}

/// Identifies the documents a restart would rebuild relations from. It covers
/// every record field [`source_document_from_record`] copies. Content enters
/// through the record content hash, which is derived from it.
fn semantic_relations_fingerprint(
    records: &HashMap<String, Arc<DocRecord>>,
    options: SupersessionOptions,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    fn field(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    fn optional(hasher: &mut Sha256, value: &Option<String>) {
        hasher.update([u8::from(value.is_some())]);
        if let Some(value) = value {
            field(hasher, value.as_bytes());
        }
    }
    let mut hasher = Sha256::new();
    field(&mut hasher, b"membrick-semantic-relations-cache");
    hasher.update(SEMANTIC_RELATIONS_CACHE_VERSION.to_le_bytes());
    field(&mut hasher, &serde_json::to_vec(&options)?);
    let mut ordered = records.values().map(AsRef::as_ref).collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    for record in ordered {
        field(&mut hasher, record.doc_id.as_bytes());
        if record.content_hash.is_empty() {
            field(&mut hasher, b"content");
            field(&mut hasher, record.content.as_bytes());
        } else {
            field(&mut hasher, b"hash");
            field(&mut hasher, record.content_hash.as_bytes());
        }
        field(&mut hasher, record.source.as_bytes());
        optional(&mut hasher, &record.group_id);
        hasher.update((record.filters.len() as u64).to_le_bytes());
        for (key, value) in &record.filters {
            field(&mut hasher, key.as_bytes());
            field(&mut hasher, value.as_bytes());
        }
        hasher.update((record.headings.len() as u64).to_le_bytes());
        for heading in &record.headings {
            field(&mut hasher, heading.as_bytes());
        }
        optional(&mut hasher, &record.probable_topic);
        hasher.update((record.doc_links.len() as u64).to_le_bytes());
        for link in &record.doc_links {
            field(&mut hasher, link.as_bytes());
        }
        optional(&mut hasher, &record.timestamp);
        hasher.update((record.doc_length as u64).to_le_bytes());
        optional(&mut hasher, &record.author_agent);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Serialize `relations` as the cache a restart would load, or `None` when the
/// live documents cannot be matched to `records`. Live documents that differ
/// from their reconstruction (for example in `concept`) are replaced by the
/// reconstruction through the incremental update, so the cached store equals
/// a full rebuild from `records`.
pub(crate) fn prepare_semantic_relations(
    relations: &SemanticRelationStore,
    source_docs: &HashMap<String, Arc<SourceDocument>>,
    records: &HashMap<String, Arc<DocRecord>>,
    options: SupersessionOptions,
) -> Result<Option<Vec<u8>>> {
    if source_docs.len() != records.len() {
        return Ok(None);
    }
    let mut reconstructed = Vec::new();
    for (doc_id, record) in records {
        let Some(doc) = source_docs.get(doc_id) else {
            return Ok(None);
        };
        if !matches_reconstruction(doc, record) {
            reconstructed.push(Arc::new(source_document_from_record(record)));
        }
    }
    let rebased;
    let relations = if reconstructed.is_empty() {
        relations
    } else {
        let mut all_docs = source_docs.clone();
        for doc in &reconstructed {
            all_docs.insert(doc.doc_id.clone(), doc.clone());
        }
        let changed = reconstructed.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        let mut store = relations.clone();
        store.update_documents_borrowed(&all_docs, &changed, &[], options)?;
        rebased = store;
        &rebased
    };
    let fingerprint = semantic_relations_fingerprint(records, options)?;
    let payload = PersistedSemanticRelationsRef {
        version: SEMANTIC_RELATIONS_CACHE_VERSION,
        fingerprint: &fingerprint,
        store: relations,
    };
    Ok(Some(oxicode::serde::encode_to_vec(
        &payload,
        oxicode::config::standard(),
    )?))
}

pub(crate) fn persist_semantic_relations(paths: &StorePaths, bytes: &[u8]) -> Result<()> {
    let Some(path) = semantic_relations_path(paths) else {
        return Ok(());
    };
    write_bytes_file_atomic(&path, bytes)
}

/// Prepare and write the relation cache. A cache that cannot be written only
/// costs the next start a rebuild, so failures are reported, not returned.
pub(crate) fn persist_semantic_relations_best_effort(
    paths: &StorePaths,
    relations: &SemanticRelationStore,
    source_docs: &HashMap<String, Arc<SourceDocument>>,
    records: &HashMap<String, Arc<DocRecord>>,
    options: SupersessionOptions,
) {
    if paths.semantic_dir.is_none() {
        return;
    }
    let result = prepare_semantic_relations(relations, source_docs, records, options)
        .and_then(|bytes| bytes.map_or(Ok(()), |bytes| persist_semantic_relations(paths, &bytes)));
    if let Err(error) = result {
        tracing::warn!("could not write semantic relation cache: {error:#}");
    }
}

/// The cached relation store, when it was built from exactly these records
/// with these options by this version of the relation code.
pub(crate) fn load_semantic_relations(
    paths: &StorePaths,
    records: &HashMap<String, Arc<DocRecord>>,
    options: SupersessionOptions,
) -> Option<SemanticRelationStore> {
    let bytes = fs::read(semantic_relations_path(paths)?).ok()?;
    let (cached, _): (PersistedSemanticRelations, _) =
        oxicode::serde::decode_from_slice(&bytes, oxicode::config::standard()).ok()?;
    if cached.version != SEMANTIC_RELATIONS_CACHE_VERSION {
        return None;
    }
    let fingerprint = semantic_relations_fingerprint(records, options).ok()?;
    (cached.fingerprint == fingerprint).then_some(cached.store)
}

/// Records are split across this many files by a hash of their doc id, so a
/// checkpoint rewrites only the files that hold a changed record.
const RECORD_SHARD_COUNT: usize = 64;
const RECORD_SHARD_DIR: &str = "records-shards";
/// Names the file that holds each shard. A checkpoint writes its changed shards
/// under new names and then replaces this file, so a checkpoint that stops
/// partway leaves the previous set whole. Until it exists, the store still
/// loads the single legacy `records.json`.
const RECORD_SHARD_MANIFEST_FILE: &str = "manifest.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct RecordShardManifest {
    schema_version: u32,
    layout_version: String,
    generation: u64,
    /// The file for each shard, or `None` for a shard with no records.
    files: Vec<Option<String>>,
}

fn record_shard_dir(store_paths: &StorePaths) -> Option<PathBuf> {
    store_paths
        .semantic_dir
        .as_ref()
        .map(|dir| dir.join(RECORD_SHARD_DIR))
}

fn record_shard_file_name(shard: usize, generation: u64) -> String {
    format!("{shard:02}-{generation}.json")
}

fn read_record_shard_manifest(shard_dir: &Path) -> Result<Option<RecordShardManifest>> {
    let manifest_path = shard_dir.join(RECORD_SHARD_MANIFEST_FILE);
    let text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let manifest: RecordShardManifest = serde_json::from_str(&text)?;
    anyhow::ensure!(
        manifest.schema_version == STORE_SCHEMA_VERSION
            && manifest.layout_version == STORE_LAYOUT_VERSION
            && manifest.files.len() == RECORD_SHARD_COUNT
            && manifest.files.iter().flatten().all(|name| {
                Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
                    && name != RECORD_SHARD_MANIFEST_FILE
            }),
        "unsupported record shard manifest at {}",
        manifest_path.display()
    );
    Ok(Some(manifest))
}

/// FNV-1a, so a record keeps its shard across processes and platforms.
fn record_shard(doc_id: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in doc_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % RECORD_SHARD_COUNT as u64) as usize
}

fn read_semantic_records(path: &Path) -> Result<PersistedSemanticRecords> {
    let persisted: PersistedSemanticRecords =
        serde_json::from_reader(std::io::BufReader::new(fs::File::open(path)?))?;
    if persisted.schema_version != STORE_SCHEMA_VERSION {
        anyhow::bail!(
            "semantic record schema mismatch at {}: found {}, expected {}",
            path.display(),
            persisted.schema_version,
            STORE_SCHEMA_VERSION
        );
    }
    if persisted.layout_version != STORE_LAYOUT_VERSION {
        anyhow::bail!(
            "semantic record layout mismatch at {}: found {}, expected {}",
            path.display(),
            persisted.layout_version,
            STORE_LAYOUT_VERSION
        );
    }
    Ok(persisted)
}

/// Records and chunk lifecycle from a complete shard set, or `None` when the
/// store has none yet.
fn read_record_shards(
    store_paths: &StorePaths,
) -> Result<Option<(Vec<PersistedDocRecord>, Vec<ChunkLifecycleMeta>)>> {
    let Some(shard_dir) = record_shard_dir(store_paths) else {
        return Ok(None);
    };
    let Some(manifest) = read_record_shard_manifest(&shard_dir)? else {
        return Ok(None);
    };
    let mut records = Vec::new();
    for name in manifest.files.iter().flatten() {
        records.extend(read_semantic_records(&shard_dir.join(name))?.records);
    }
    let lifecycle = match chunk_lifecycle_path(store_paths) {
        Some(path) if path.exists() => serde_json::from_str(&fs::read_to_string(path)?)?,
        _ => Vec::new(),
    };
    Ok(Some((records, lifecycle)))
}

/// Every persisted record in a semantic directory, from its shard set or the
/// legacy single file.
pub(crate) fn load_persisted_records(semantic_dir: &Path) -> Result<Vec<DocRecord>> {
    let paths = StorePaths {
        root: None,
        semantic_dir: Some(semantic_dir.to_path_buf()),
        lexical_dir: None,
        metadata_path: None,
    };
    let records = match read_record_shards(&paths)? {
        Some((records, _)) => records,
        None => read_semantic_records(&semantic_dir.join(SEMANTIC_RECORDS_FILE))?.records,
    };
    Ok(records.into_iter().map(Into::into).collect())
}

type SemanticState = (
    HashMap<String, Arc<SourceDocument>>,
    HashMap<String, Arc<DocRecord>>,
    HashMap<String, ChunkLifecycleMeta>,
    Option<MemoryIndex>,
    // Whether the records came from a shard set, so they match the files on
    // disk and a checkpoint may rewrite only the shards that change.
    bool,
);

pub(crate) fn load_semantic_state(
    store_paths: &StorePaths,
    layout: &MemoryIndexLayout,
) -> Result<SemanticState> {
    let empty = || Ok((HashMap::new(), HashMap::new(), HashMap::new(), None, false));
    let Some(records_path) = semantic_records_path(store_paths) else {
        return empty();
    };
    let sharded = read_record_shards(store_paths)?;
    if sharded.is_none() && !records_path.exists() {
        return empty();
    }
    let core_path = semantic_core_path(store_paths);
    if matches!(layout, MemoryIndexLayout::Single)
        && !core_path.as_ref().is_some_and(|p| p.exists())
    {
        return empty();
    }

    let from_shards = sharded.is_some();
    let (persisted_records, persisted_lifecycle) = match sharded {
        Some(shards) => shards,
        None => {
            let persisted = read_semantic_records(&records_path)?;
            (persisted.records, persisted.chunk_lifecycle)
        }
    };
    let restored_records = persisted_records
        .into_iter()
        .map(Into::into)
        .collect::<Vec<DocRecord>>();
    let snapshot = match layout {
        MemoryIndexLayout::Single => Some(MemoryIndex::load_with_binary_core(
            restored_records.clone(),
            core_path
                .as_ref()
                .expect("single layout requires a core path"),
            None,
            false,
        )?),
        MemoryIndexLayout::Segmented { .. } | MemoryIndexLayout::AdaptiveSegmented { .. } => None,
    };
    let mut source_docs = HashMap::new();
    let mut records = HashMap::new();
    for record in restored_records {
        source_docs.insert(
            record.doc_id.clone(),
            Arc::new(source_document_from_record(&record)),
        );
        records.insert(record.doc_id.clone(), Arc::new(record));
    }
    let mut chunk_lifecycle = persisted_lifecycle
        .into_iter()
        .map(|meta| (meta.chunk_id.clone(), meta))
        .collect::<HashMap<_, _>>();
    if chunk_lifecycle.is_empty() {
        for record in records.values().map(AsRef::as_ref) {
            for chunk in &record.section_chunks {
                let chunk_id = chunk.chunk_id.clone();
                chunk_lifecycle.insert(
                    chunk_id.clone(),
                    ChunkLifecycleMeta {
                        chunk_id,
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
    } else if let Some(chunk_lifecycle_path) = chunk_lifecycle_path(store_paths) {
        if !chunk_lifecycle_path.exists() {
            write_text_file_atomic(
                &chunk_lifecycle_path,
                &serde_json::to_string_pretty(
                    &chunk_lifecycle.values().cloned().collect::<Vec<_>>(),
                )?,
            )?;
        }
    }
    Ok((source_docs, records, chunk_lifecycle, snapshot, from_shards))
}

pub(crate) struct PreparedSemanticState {
    // Internal transport only; each shard on disk remains ordinary JSON.
    // `None` is a shard left with no records, which gets no file.
    shards: Vec<(usize, Option<Vec<u8>>)>,
    /// Every shard is included, so the set can be marked complete and the
    /// legacy single file retired.
    complete: bool,
    lifecycle: String,
    core: Option<Vec<u8>>,
}

/// One shard's records as compressed JSON.
fn encode_record_shard(mut records: Vec<&DocRecord>) -> Result<Vec<u8>> {
    #[derive(serde::Serialize)]
    struct RecordRef<'a> {
        #[serde(flatten)]
        record: &'a DocRecord,
        // DocRecord skips this field in its public serialization. The existing
        // persistent format includes it, so restore it without cloning it.
        content: &'a str,
    }
    #[derive(serde::Serialize)]
    struct RecordsRef<'a> {
        schema_version: u32,
        layout_version: &'a str,
        records: Vec<RecordRef<'a>>,
    }
    records.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    let payload = RecordsRef {
        schema_version: STORE_SCHEMA_VERSION,
        layout_version: STORE_LAYOUT_VERSION,
        records: records
            .into_iter()
            .map(|record| RecordRef {
                record,
                content: &record.content,
            })
            .collect(),
    };
    // Stream directly into the encoder: never materialize the uncompressed
    // JSON in a second heap buffer while the immutable records live.
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)?;
    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, encoder);
    serde_json::to_writer(&mut buffered, &payload)?;
    let encoder = buffered.into_inner().map_err(|error| error.into_error())?;
    Ok(encoder.finish()?)
}

/// Serialize a matching generation without touching the filesystem.
///
/// `persisted` is the record map whose shards are already on disk. Only
/// shards holding an added, changed or removed record are serialized; with
/// no map, every shard is.
pub(crate) fn prepare_semantic_state(
    snapshot: Option<&MemoryIndex>,
    records_map: &HashMap<String, Arc<DocRecord>>,
    chunk_lifecycle_map: &HashMap<String, ChunkLifecycleMeta>,
    persisted: Option<&HashMap<String, Arc<DocRecord>>>,
) -> Result<PreparedSemanticState> {
    let mut changed = vec![persisted.is_none(); RECORD_SHARD_COUNT];
    if let Some(persisted) = persisted {
        for (doc_id, record) in records_map {
            if !persisted
                .get(doc_id)
                .is_some_and(|previous| Arc::ptr_eq(previous, record))
            {
                changed[record_shard(doc_id)] = true;
            }
        }
        for doc_id in persisted.keys() {
            if !records_map.contains_key(doc_id) {
                changed[record_shard(doc_id)] = true;
            }
        }
    }
    let mut grouped = vec![Vec::new(); RECORD_SHARD_COUNT];
    for (doc_id, record) in records_map {
        let shard = record_shard(doc_id);
        if changed[shard] {
            grouped[shard].push(record.as_ref());
        }
    }
    let shards = grouped
        .into_iter()
        .enumerate()
        .filter(|(shard, _)| changed[*shard])
        .map(|(shard, records)| {
            let bytes = (!records.is_empty())
                .then(|| encode_record_shard(records))
                .transpose()?;
            Ok((shard, bytes))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut lifecycle = chunk_lifecycle_map.values().collect::<Vec<_>>();
    lifecycle.sort_by(|a, b| a.chunk_id.cmp(&b.chunk_id));
    Ok(PreparedSemanticState {
        shards,
        complete: persisted.is_none(),
        lifecycle: serde_json::to_string(&lifecycle)?,
        core: snapshot.map(MemoryIndex::to_bytes).transpose()?,
    })
}

pub(crate) fn persist_prepared_semantic_state(
    paths: &StorePaths,
    prepared: &PreparedSemanticState,
) -> Result<()> {
    let Some(semantic_dir) = paths.semantic_dir.as_ref() else {
        return Ok(());
    };
    let shard_dir = record_shard_dir(paths).expect("shard dir");
    fs::create_dir_all(&shard_dir)?;
    let previous = read_record_shard_manifest(&shard_dir)?;
    anyhow::ensure!(
        prepared.complete || previous.is_some(),
        "partial checkpoint without a record shard manifest at {}",
        shard_dir.display()
    );
    let generation = previous.as_ref().map_or(0, |manifest| manifest.generation) + 1;
    let mut files = match previous {
        Some(manifest) if !prepared.complete => manifest.files,
        _ => vec![None; RECORD_SHARD_COUNT],
    };
    for (shard, bytes) in &prepared.shards {
        files[*shard] = match bytes {
            Some(bytes) => {
                let name = record_shard_file_name(*shard, generation);
                write_file_atomic_with(&shard_dir.join(&name), false, |file| {
                    let mut decoder = zstd::stream::read::Decoder::new(bytes.as_slice())?;
                    std::io::copy(&mut decoder, file)?;
                    Ok(())
                })?;
                Some(name)
            }
            None => None,
        };
    }
    write_text_file_atomic(
        &chunk_lifecycle_path(paths).expect("lifecycle path"),
        &prepared.lifecycle,
    )?;
    if let Some(core) = &prepared.core {
        write_bytes_file_atomic(&semantic_core_path(paths).expect("core path"), core)?;
    }
    let manifest = RecordShardManifest {
        schema_version: STORE_SCHEMA_VERSION,
        layout_version: STORE_LAYOUT_VERSION.to_string(),
        generation,
        files,
    };
    write_text_file_atomic(
        &shard_dir.join(RECORD_SHARD_MANIFEST_FILE),
        &serde_json::to_string(&manifest)?,
    )?;
    // Shards the manifest no longer names, including any left by a checkpoint
    // that stopped before its manifest was written.
    let referenced = manifest
        .files
        .iter()
        .flatten()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    for entry in fs::read_dir(&shard_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".json")
            && name != RECORD_SHARD_MANIFEST_FILE
            && !referenced.contains(name)
        {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    if prepared.complete {
        match fs::remove_file(semantic_dir.join(SEMANTIC_RECORDS_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn persist_semantic_state(
    paths: &StorePaths,
    snapshot: Option<&MemoryIndex>,
    records: &HashMap<String, Arc<DocRecord>>,
    lifecycle: &HashMap<String, ChunkLifecycleMeta>,
    persisted: Option<&HashMap<String, Arc<DocRecord>>>,
) -> Result<()> {
    if paths.semantic_dir.is_none() {
        return Ok(());
    }
    let prepared = prepare_semantic_state(snapshot, records, lifecycle, persisted)?;
    persist_prepared_semantic_state(paths, &prepared)
}

pub(crate) fn ensure_safe_output_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        anyhow::bail!("refusing to write: output path is a directory");
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing to write: output path is a symlink");
        }
    }
    if let Some(parent) = path.parent() {
        let mut cur = if parent.is_absolute() {
            PathBuf::from("/")
        } else {
            std::env::current_dir()?
        };
        for comp in parent.components() {
            use std::path::Component;
            match comp {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => {
                    anyhow::bail!("refusing to write: parent traversal is not allowed")
                }
                Component::Normal(seg) => {
                    cur.push(seg);
                    if let Ok(meta) = fs::symlink_metadata(&cur) {
                        if meta.file_type().is_symlink() {
                            anyhow::bail!(
                                "refusing to write: parent path component is a symlink ({})",
                                cur.display()
                            );
                        }
                    }
                }
                Component::Prefix(_) => {}
            }
        }
    }
    Ok(())
}

fn atomic_temp_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!(".{}.{}.{}.tmp", stem, pid, nanos + attempt as u128));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "unable to allocate temporary file path for {}",
        path.display()
    )
}

pub(crate) fn write_text_file_atomic(path: &Path, content: &str) -> Result<()> {
    write_text_file_atomic_with_permissions(path, content, false)
}

pub(crate) fn write_private_text_file_atomic(path: &Path, content: &str) -> Result<()> {
    write_text_file_atomic_with_permissions(path, content, true)
}

fn write_text_file_atomic_with_permissions(
    path: &Path,
    content: &str,
    private: bool,
) -> Result<()> {
    write_bytes_file_atomic_with_permissions(path, content.as_bytes(), private)
}

pub(crate) fn write_bytes_file_atomic(path: &Path, content: &[u8]) -> Result<()> {
    write_bytes_file_atomic_with_permissions(path, content, false)
}

fn write_bytes_file_atomic_with_permissions(
    path: &Path,
    content: &[u8],
    private: bool,
) -> Result<()> {
    write_file_atomic_with(path, private, |file| {
        file.write_all(content)?;
        Ok(())
    })
}

fn write_file_atomic_with(
    path: &Path,
    private: bool,
    write: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<()> {
    ensure_safe_output_path(path)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let temp_path = atomic_temp_path(path)?;
    let mut created = false;
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&temp_path)?;
        created = true;
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&temp_path);
    }
    result?;
    // Unix permits opening and syncing a directory to make the rename durable.
    // Windows does not expose directory fsync through `File::open`; doing so
    // fails with ERROR_ACCESS_DENIED after the destination was already replaced.
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod checkpoint_buffer_tests {
    use super::*;

    /// Allocation regression: retained checkpoint bytes must compress repeated
    /// captured text. Round-trip assertions cover the unchanged on-disk format.
    #[test]
    fn prepared_checkpoint_does_not_retain_another_full_json_copy() {
        let content = "quoted \" text\n café ☃ ".repeat(200_000);
        let record: DocRecord = serde_json::from_value(serde_json::json!({
            "doc_id":"large", "source":"test://large", "content":content,
            "timestamp":null, "doc_length":content.len(), "author_agent":null,
            "group_id":null, "probable_topic":null, "doc_type_guess":null,
            "headings":[], "key_entities":[], "important_terms":[],
            "section_chunks":[], "embedding":null, "top_claims":[],
            "provenance":{"source":"test://large", "timestamp":null,
                "ner_provider":"heuristic", "term_ranker":"yake", "index_version":"test"}
        }))
        .unwrap();
        let records = HashMap::from([("large".to_string(), Arc::new(record))]);
        let mut prepared = prepare_semantic_state(None, &records, &HashMap::new(), None).unwrap();
        let retained = prepared
            .shards
            .iter()
            .flat_map(|(_, bytes)| bytes.as_ref().map(Vec::len))
            .sum::<usize>();
        assert!(
            retained < content.len() / 10,
            "checkpoint retained {retained} bytes for {} bytes of repeated source text",
            content.len()
        );
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mb-checkpoint-buffer-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let paths = StorePaths {
            root: Some(root.clone()),
            semantic_dir: Some(root.clone()),
            lexical_dir: None,
            metadata_path: None,
        };
        persist_prepared_semantic_state(&paths, &prepared).unwrap();
        let shard_dir = record_shard_dir(&paths).unwrap();
        let manifest = read_record_shard_manifest(&shard_dir).unwrap().unwrap();
        assert_eq!(
            manifest.files.iter().flatten().count(),
            1,
            "a shard with no records got a file"
        );
        let shard_path = shard_dir.join(manifest.files[record_shard("large")].as_ref().unwrap());
        let payload: serde_json::Value =
            serde_json::from_reader(fs::File::open(&shard_path).unwrap()).unwrap();
        assert_eq!(
            payload["records"][0]["content"].as_str(),
            Some(content.as_str())
        );
        assert_eq!(payload["schema_version"], STORE_SCHEMA_VERSION);
        assert_eq!(payload["layout_version"], STORE_LAYOUT_VERSION);
        assert_eq!(
            fs::read_to_string(chunk_lifecycle_path(&paths).unwrap()).unwrap(),
            "[]"
        );
        let prior = fs::read(&shard_path).unwrap();
        let (_, bytes) = prepared
            .shards
            .iter_mut()
            .find(|(shard, _)| *shard == record_shard("large"))
            .unwrap();
        bytes.as_mut().unwrap().truncate(8);
        assert!(persist_prepared_semantic_state(&paths, &prepared).is_err());
        assert_eq!(
            fs::read(&shard_path).unwrap(),
            prior,
            "failed streaming write replaced a valid checkpoint"
        );
        assert_eq!(
            fs::read_dir(&shard_dir).unwrap().count(),
            2,
            "failed streaming write left a temporary output file"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn record(doc_id: &str, content: &str) -> Arc<DocRecord> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "doc_id":doc_id, "source":format!("test://{doc_id}"), "content":content,
                "timestamp":null, "doc_length":content.len(), "author_agent":null,
                "group_id":null, "probable_topic":null, "doc_type_guess":null,
                "headings":[], "key_entities":[], "important_terms":[],
                "section_chunks":[], "embedding":null, "top_claims":[],
                "provenance":{"source":format!("test://{doc_id}"), "timestamp":null,
                    "ner_provider":"heuristic", "term_ranker":"yake", "index_version":"test"}
            }))
            .unwrap(),
        )
    }

    fn temp_paths(name: &str) -> StorePaths {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        StorePaths {
            root: Some(root.clone()),
            semantic_dir: Some(root),
            lexical_dir: None,
            metadata_path: None,
        }
    }

    fn loaded_contents(paths: &StorePaths) -> (HashMap<String, String>, bool) {
        let (_, records, _, _, from_shards) = load_semantic_state(
            paths,
            &MemoryIndexLayout::Segmented {
                query_top_n: 4,
                routing_strategy: crate::segments::SegmentRoutingStrategy::SparseOverlap,
            },
        )
        .unwrap();
        let contents = records
            .into_iter()
            .map(|(doc_id, record)| (doc_id, record.content.to_string()))
            .collect();
        (contents, from_shards)
    }

    #[test]
    fn checkpoint_rewrites_only_the_shards_that_changed() {
        let paths = temp_paths("checkpoint-shards");
        let persisted = (0..200)
            .map(|i| (format!("doc-{i}"), record(&format!("doc-{i}"), "first")))
            .collect::<HashMap<_, _>>();
        persist_semantic_state(&paths, None, &persisted, &HashMap::new(), None).unwrap();

        let mut records = persisted.clone();
        records.insert("doc-7".into(), record("doc-7", "second"));
        records.remove("doc-42");
        records.insert("doc-new".into(), record("doc-new", "added"));
        let prepared =
            prepare_semantic_state(None, &records, &HashMap::new(), Some(&persisted)).unwrap();

        let mut written = prepared
            .shards
            .iter()
            .map(|(shard, _)| *shard)
            .collect::<Vec<_>>();
        written.sort();
        let mut expected = vec![
            record_shard("doc-7"),
            record_shard("doc-42"),
            record_shard("doc-new"),
        ];
        expected.sort();
        expected.dedup();
        assert_eq!(written, expected, "checkpoint rewrote unchanged shards");
        assert!(!prepared.complete);

        persist_prepared_semantic_state(&paths, &prepared).unwrap();
        let (contents, from_shards) = loaded_contents(&paths);
        assert!(from_shards);
        assert_eq!(contents.len(), 200);
        assert_eq!(contents["doc-7"], "second");
        assert_eq!(contents["doc-new"], "added");
        assert!(!contents.contains_key("doc-42"));
        fs::remove_dir_all(paths.root.unwrap()).unwrap();
    }

    /// A checkpoint that stops after writing some of its shards must leave the
    /// previous checkpoint whole, not a mix of new and old shards.
    #[test]
    fn checkpoint_stopped_partway_keeps_the_previous_records() {
        let paths = temp_paths("checkpoint-partial");
        let persisted = (0..200)
            .map(|i| (format!("doc-{i}"), record(&format!("doc-{i}"), "first")))
            .collect::<HashMap<_, _>>();
        persist_semantic_state(&paths, None, &persisted, &HashMap::new(), None).unwrap();

        let records = persisted
            .keys()
            .map(|doc_id| (doc_id.clone(), record(doc_id, "second")))
            .collect::<HashMap<_, _>>();
        let mut prepared =
            prepare_semantic_state(None, &records, &HashMap::new(), Some(&persisted)).unwrap();
        assert!(prepared.shards.len() > 1);
        // The last shard fails to write, after the others are on disk.
        let (_, bytes) = prepared.shards.last_mut().unwrap();
        bytes.as_mut().unwrap().truncate(8);
        assert!(persist_prepared_semantic_state(&paths, &prepared).is_err());

        let (contents, from_shards) = loaded_contents(&paths);
        assert!(from_shards);
        assert_eq!(contents.len(), 200);
        assert!(
            contents.values().all(|content| content == "first"),
            "a stopped checkpoint mixed new shards into the previous one"
        );

        // The next checkpoint completes and removes what the stopped one left.
        let prepared =
            prepare_semantic_state(None, &records, &HashMap::new(), Some(&persisted)).unwrap();
        persist_prepared_semantic_state(&paths, &prepared).unwrap();
        let (contents, _) = loaded_contents(&paths);
        assert!(contents.values().all(|content| content == "second"));
        let shard_dir = record_shard_dir(&paths).unwrap();
        let manifest = read_record_shard_manifest(&shard_dir).unwrap().unwrap();
        assert_eq!(
            fs::read_dir(&shard_dir).unwrap().count(),
            manifest.files.iter().flatten().count() + 1,
            "unreferenced shard files were left behind"
        );
        fs::remove_dir_all(paths.root.unwrap()).unwrap();
    }

    #[test]
    fn first_checkpoint_moves_a_legacy_records_file_into_shards() {
        let paths = temp_paths("checkpoint-legacy");
        let records = (0..20)
            .map(|i| (format!("doc-{i}"), record(&format!("doc-{i}"), "legacy")))
            .collect::<HashMap<_, _>>();
        let legacy = serde_json::json!({
            "schema_version": STORE_SCHEMA_VERSION,
            "layout_version": STORE_LAYOUT_VERSION,
            "records": records.values().map(|record| {
                let mut value = serde_json::to_value(record.as_ref()).unwrap();
                value["content"] = serde_json::Value::String(record.content.to_string());
                value
            }).collect::<Vec<_>>(),
        });
        let legacy_path = semantic_records_path(&paths).unwrap();
        fs::write(&legacy_path, legacy.to_string()).unwrap();

        let (contents, from_shards) = loaded_contents(&paths);
        assert!(!from_shards, "a legacy file is not a shard set");
        assert_eq!(contents.len(), 20);

        persist_semantic_state(&paths, None, &records, &HashMap::new(), None).unwrap();
        assert!(!legacy_path.exists(), "the legacy file outlived its shards");
        let (contents, from_shards) = loaded_contents(&paths);
        assert!(from_shards);
        assert_eq!(contents.len(), 20);
        assert!(contents.values().all(|content| content == "legacy"));
        fs::remove_dir_all(paths.root.unwrap()).unwrap();
    }
}

/// Every persisted record as JSON, from the shard set or the legacy file.
#[cfg(test)]
pub(crate) fn persisted_record_values(semantic_dir: &Path) -> Vec<serde_json::Value> {
    let read = |path: &Path| -> serde_json::Value {
        serde_json::from_reader(fs::File::open(path).unwrap()).unwrap()
    };
    let shard_dir = semantic_dir.join(RECORD_SHARD_DIR);
    let mut records = Vec::new();
    if let Some(manifest) = read_record_shard_manifest(&shard_dir).unwrap() {
        for name in manifest.files.iter().flatten() {
            let payload = read(&shard_dir.join(name));
            records.extend(payload["records"].as_array().unwrap().iter().cloned());
        }
    } else {
        let payload = read(&semantic_dir.join(SEMANTIC_RECORDS_FILE));
        records.extend(payload["records"].as_array().unwrap().iter().cloned());
    }
    records.sort_by(|a, b| a["doc_id"].as_str().cmp(&b["doc_id"].as_str()));
    records
}
