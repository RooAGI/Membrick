//! Memory search: filtered retrieval, structured facts, and result blending.

use super::*;

impl MemoryService {
    /// The API-facing search entry point (HTTP server, Python bindings).
    ///
    /// This is a thin wrapper — it is NOT a separate retrieval path. All
    /// retrieval logic lives in [`MemoryService::search_with_filters`], the
    /// single search implementation. This function only translates the
    /// [`SearchRequest`] into the filter/scope args and shapes the raw hits
    /// into a [`SearchResponse`].
    ///
    /// Single-path guarantee: any change to retrieval behavior must go in
    /// `search_with_filters`, never here. This wrapper must stay logic-free
    /// (argument translation + response shaping only) so the HTTP/Python
    /// path and the MCP/benchmark path can never diverge.
    pub fn search(&mut self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
        self.search_impl(request)
    }

    /// Search the already-published index snapshot. Server callers use this
    /// under a shared service lock after mutations have refreshed the index.
    pub fn search_cached(&self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
        self.search_impl_cached(request)
    }

    fn search_impl(&mut self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
        if !request.user_id.trim().is_empty() {
            validate_identifier(&request.user_id, "user_id")?;
        }
        if let Some(session_id) = request.session_id.as_deref() {
            validate_identifier(session_id, "session_id")?;
        }
        if request.query.trim().is_empty() {
            return Ok(SearchResponse { data: vec![] });
        }
        // The conversation-state scope defaults to the user; an explicit
        // scope overrides it (e.g. the benchmark shares one scope across
        // many document owners).
        let scope = request
            .scope
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| request.user_id.clone());
        // Build the document filters: ownership (unless empty, for
        // provider-scoped callers) plus any caller-supplied extras.
        let mut filters = BTreeMap::new();
        if !request.user_id.trim().is_empty() {
            filters.insert(USER_FILTER.to_string(), request.user_id.clone());
        }
        if let Some(extra) = request.filters.as_ref() {
            for (key, value) in extra {
                if key != USER_FILTER {
                    filters.insert(key.clone(), value.clone());
                }
            }
        }
        let results = if self.requires_visibility_allow_list() {
            let allowed_doc_ids = self.visible_doc_ids_matching(&filters);
            self.search_with_filters_restricted(
                &request.query,
                &scope,
                request.session_id.as_deref(),
                request.top_k,
                &filters,
                request.reference_date.as_deref(),
                &allowed_doc_ids,
            )?
        } else {
            self.search_with_filters(
                &request.query,
                &scope,
                request.session_id.as_deref(),
                request.top_k,
                &filters,
                request.reference_date.as_deref(),
            )?
        };
        Ok(self.format_search_response(results))
    }

    fn search_impl_cached(&self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
        if !request.user_id.trim().is_empty() {
            validate_identifier(&request.user_id, "user_id")?;
        }
        if let Some(session_id) = request.session_id.as_deref() {
            validate_identifier(session_id, "session_id")?;
        }
        if request.query.trim().is_empty() {
            return Ok(SearchResponse { data: vec![] });
        }
        let scope = request
            .scope
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| request.user_id.clone());
        let mut filters = BTreeMap::new();
        if !request.user_id.trim().is_empty() {
            filters.insert(USER_FILTER.to_string(), request.user_id.clone());
        }
        if let Some(extra) = request.filters.as_ref() {
            for (key, value) in extra {
                if key != USER_FILTER {
                    filters.insert(key.clone(), value.clone());
                }
            }
        }
        let results = if self.requires_visibility_allow_list() {
            let allowed_doc_ids = self.visible_doc_ids_matching(&filters);
            self.search_with_filters_cached_and_allowed_doc_ids(
                &request.query,
                &scope,
                request.session_id.as_deref(),
                request.top_k,
                &filters,
                request.reference_date.as_deref(),
                Some(&allowed_doc_ids),
            )?
        } else {
            self.search_with_filters_cached(
                &request.query,
                &scope,
                request.session_id.as_deref(),
                request.top_k,
                &filters,
                request.reference_date.as_deref(),
            )?
        };
        Ok(self.format_search_response(results))
    }

    pub(super) fn is_visible(&self, doc: &SourceDocument) -> bool {
        let not_superseded = doc
            .filters
            .get(USER_FILTER)
            .map(|user| (user.clone(), doc.doc_id.clone()))
            .is_none_or(|key| !self.superseded_ids.contains(&key));
        let not_expired = doc
            .filters
            .get("expires_at_ms")
            .and_then(|value| value.parse::<u64>().ok())
            .is_none_or(|expires_at| expires_at > unix_time_ms());
        not_superseded && not_expired
    }

    pub(super) fn requires_visibility_allow_list(&self) -> bool {
        // Keep expired and API-superseded documents out before ranking when
        // either lifecycle feature is present. Ordinary stores can use the
        // index's exact-filter bitmap without materializing every visible ID.
        self.has_expiring_documents
            || self.has_session_observations
            || !self.superseded_ids.is_empty()
    }

    fn visible_doc_ids_matching(&self, filters: &BTreeMap<String, String>) -> HashSet<String> {
        self.store
            .source_documents()
            .into_iter()
            .filter(|document| {
                self.is_visible(document)
                    && !is_session_observation(document)
                    && filters.iter().all(|(key, value)| {
                        document
                            .filters
                            .get(key)
                            .is_some_and(|actual| actual == value)
                    })
            })
            .map(|document| document.doc_id.clone())
            .collect()
    }

    /// The single search implementation. ALL retrieval goes through this
    /// function — the MCP `search` tools (Claude Code, Codex, Gemini CLI,
    /// Muse), the benchmark harnesses, and [`MemoryService::search`] (thin
    /// wrapper for the HTTP/Python API) all converge here.
    ///
    /// Pipeline: query-time key-phrase backfill → conversational query
    /// preparation → lexical retrieval with optional conversational rerank →
    /// structured-fact retrieval blended ahead of lexical → session-state
    /// observation. Returns raw hits; callers shape them via
    /// [`MemoryService::format_search_response`] (API) or the shared
    /// `search_results` formatter (agent-facing).
    ///
    /// Single-path guarantee: this is the ONLY place retrieval behavior may
    /// change. Do not add a second implementation — extend this one.
    pub fn search_with_filters(
        &mut self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
        filters: &BTreeMap<String, String>,
        reference_date: Option<&str>,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        // Query-time key-phrase backfill: documents written by provider
        // hooks (separate short-lived processes whose background workers
        // die with them) get their phrases extracted synchronously here, so
        // this query benefits. Bounded to one extractor batch and fail-open;
        // usually a no-op once every document carries its extraction stamp.
        self.backfill_key_phrases();
        self.flush()?;
        if self.requires_visibility_allow_list() {
            let allowed_doc_ids = self.visible_doc_ids_matching(filters);
            self.search_with_filters_cached_and_allowed_doc_ids(
                query,
                scope,
                session_id,
                top_k,
                filters,
                reference_date,
                Some(&allowed_doc_ids),
            )
        } else {
            self.search_with_filters_cached(
                query,
                scope,
                session_id,
                top_k,
                filters,
                reference_date,
            )
        }
    }

    fn search_with_filters_restricted(
        &mut self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
        filters: &BTreeMap<String, String>,
        reference_date: Option<&str>,
        allowed_doc_ids: &HashSet<String>,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        self.backfill_key_phrases();
        self.flush()?;
        self.search_with_filters_cached_and_allowed_doc_ids(
            query,
            scope,
            session_id,
            top_k,
            filters,
            reference_date,
            Some(allowed_doc_ids),
        )
    }

    /// Session tag link expansion (Luyi 2026-10-07).
    ///
    /// When a document is boosted via its semantic tags, other documents in
    /// the same session that share those tags get a boost too. This uses the
    /// tag->doc provenance from the session's documents (supplements the
    /// per-document tag BM25 matching, does not replace it).
    ///
    /// For example: session has doc-1 (tags: weekend, habitual) and doc-3
    /// (tags: habitual). Query "weekends" boosts doc-1 via "weekend" tag.
    /// doc-1's "habitual" tag links to doc-3, so doc-3 gets a boost too.
    fn apply_session_tag_link_boost(
        &self,
        mut results: Vec<crate::SearchResult>,
        session_id: Option<&str>,
        query: &str,
    ) -> Vec<crate::SearchResult> {
        if results.is_empty() {
            return results;
        }
        // Tag-only re-rank (Luyi 2026-10-07): Phase 2 of two-stage retrieval.
        // Phase 1 (BM25) established the candidate set via "user" (preference
        // marker). Phase 2 ranks WITHIN the set by semantic_tags overlap only.
        // Definitional knowledge (tags) is the ranking signal, not text similarity.
        // This is deterministic: no BM25 variance.
        {
            // Known tag vocabulary (must match TAG_BASES in index/build.rs).
            const TAG_VOCAB: &[&str] = &["weekend", "weekday", "habitual", "herb", "food", "place"];
            let query_lower = query.to_lowercase();
            let query_tags: Vec<&str> = TAG_VOCAB
                .iter()
                .filter(|t| query_lower.contains(**t))
                .copied()
                .collect();
            if !query_tags.is_empty() {
                // Score each result by tag overlap count.
                let mut scored: Vec<(usize, usize, f32)> = Vec::new(); // (idx, overlap, orig_score)
                for (idx, result) in results.iter().enumerate() {
                    let overlap = self
                        .record_by_id(&result.doc_id)
                        .map(|r| {
                            r.semantic_tags
                                .iter()
                                .filter(|t| query_tags.contains(&t.as_str()))
                                .count()
                        })
                        .unwrap_or(0);
                    scored.push((idx, overlap, result.score));
                }
                // Sort by overlap (desc), then original score (desc) for ties.
                scored.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
                });
                // Reorder results.
                let reordered: Vec<crate::SearchResult> = scored
                    .into_iter()
                    .map(|(idx, _, _)| results[idx].clone())
                    .collect();
                results = reordered;
            }
        }
        // Question-type routing (Luyi 2026-10-07): "where" -> venue kind,
        // "when" -> temporal. Boost docs whose semantic_tags contain the
        // relevant category. This is systematic, not per-question.
        let query_lower = query.to_lowercase();
        let is_where = query_lower.trim_start().starts_with("where");
        let is_when = query_lower.trim_start().starts_with("when");
        // Build tag->docs mapping from the session's documents.
        // We use the in-memory DocRecords (via record_by_id) to avoid a
        // Tantivy lookup. The session_doc in the index has the same data
        // persisted, but in-memory is faster for query-time.
        use std::collections::{HashMap, HashSet};
        let mut tag_to_docs: HashMap<String, Vec<String>> = HashMap::new();
        let mut doc_to_tags: HashMap<String, Vec<String>> = HashMap::new();

        // Get all docs in this session by scanning results' sessions.
        // Actually, we need ALL docs in the session, not just results.
        // For now, use the top results' docs to build the mapping.
        // A full implementation would fetch all session docs from the store.
        for result in &results {
            if let Some(record) = self.record_by_id(&result.doc_id) {
                // If session_id given, filter to that session. Otherwise include all.
                if let Some(sid) = session_id {
                    if record
                        .group_id
                        .as_deref()
                        .map(crate::provider_sources::session_group)
                        != Some(sid)
                    {
                        continue;
                    }
                }
                for tag in &record.semantic_tags {
                    tag_to_docs
                        .entry(tag.clone())
                        .or_default()
                        .push(result.doc_id.clone());
                    doc_to_tags
                        .entry(result.doc_id.clone())
                        .or_default()
                        .push(tag.clone());
                }
            }
        }

        // Question-type boosting (Luyi 2026-10-07): "where" -> venue, "when" -> temporal.
        const QUESTION_TYPE_BOOST: f32 = 1.5;
        if is_where || is_when {
            for result in &mut results {
                if let Some(tags) = doc_to_tags.get(&result.doc_id) {
                    let has_relevant = if is_where {
                        tags.iter().any(|t| t == "venue")
                    } else {
                        tags.iter()
                            .any(|t| matches!(t.as_str(), "weekend" | "weekday" | "habitual"))
                    };
                    if has_relevant {
                        result.score *= QUESTION_TYPE_BOOST;
                    }
                }
            }
        }

        if tag_to_docs.is_empty() {
            if is_where || is_when {
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            return results;
        }

        // For each result doc, find co-linked docs via shared tags and boost them.
        // Boost factor: 1.2x for docs sharing at least one tag with a top result.
        const LINK_BOOST: f32 = 1.2;
        let top_doc_ids: HashSet<String> =
            results.iter().take(5).map(|r| r.doc_id.clone()).collect();
        let mut boost_map: HashMap<String, f32> = HashMap::new();

        for doc_id in &top_doc_ids {
            if let Some(tags) = doc_to_tags.get(doc_id) {
                for tag in tags {
                    if let Some(linked_docs) = tag_to_docs.get(tag) {
                        for linked_doc in linked_docs {
                            if !top_doc_ids.contains(linked_doc) {
                                // This doc shares a tag with a top result but isn't in top results.
                                // Mark it for boosting if it appears in the full results list.
                                boost_map.insert(linked_doc.clone(), LINK_BOOST);
                            }
                        }
                    }
                }
            }
        }

        // Apply boosts to results.
        for result in &mut results {
            if let Some(boost) = boost_map.get(&result.doc_id) {
                result.score *= boost;
            }
        }

        // Re-sort by score (boost may have changed order).
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    /// Run retrieval against the published immutable index. All mutable
    /// side state used by search is internally synchronized, so this path can
    /// serve concurrent readers under a shared service lock.
    pub(super) fn search_with_filters_cached(
        &self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
        filters: &BTreeMap<String, String>,
        reference_date: Option<&str>,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        self.search_with_filters_cached_and_allowed_doc_ids(
            query,
            scope,
            session_id,
            top_k,
            filters,
            reference_date,
            None,
        )
    }

    fn search_with_filters_cached_and_allowed_doc_ids(
        &self,
        query: &str,
        scope: &str,
        session_id: Option<&str>,
        top_k: usize,
        filters: &BTreeMap<String, String>,
        reference_date: Option<&str>,
        additional_allowed_doc_ids: Option<&HashSet<String>>,
    ) -> anyhow::Result<Vec<crate::SearchResult>> {
        // Prepare the question once. Session follow-ups must be resolved from
        // the raw question before PreparedQuery derives its analysis and
        // augmented lexical search text.
        let reasoning_rules = self
            .project_rules
            .as_ref()
            .map(|rules| rules.snapshot())
            .unwrap_or_else(crate::reasoning_rules::builtin_rules);
        let prepared = if session_id.is_none() {
            // Reference date (Luyi 2026-10-09): when supplied, relative
            // temporal language resolves against it instead of the clock.
            match reference_date {
                Some(ref_date) => {
                    PreparedQuery::new_at_with_rules(query, ref_date, Arc::clone(&reasoning_rules))
                }
                None => PreparedQuery::new_with_rules(query, Arc::clone(&reasoning_rules)),
            }
        } else {
            prepare_session_query(
                &self.conversation_states,
                scope,
                session_id,
                query,
                reference_date,
                Arc::clone(&reasoning_rules),
            )
        };
        // Synthetic document terms (Luyi 2026-10-07): bekind's mapping lives
        // in the index, not in the query. Each document's canonical
        // definitional tags (closed-set temporal words, "habitual", admitted
        // kind tags) are indexed as ordinary terms in the `semantic_tags`
        // field, which the tantivy QueryParser searches like every other
        // field. The query itself needs no per-query bekind mapping: a
        // question saying "weekend" matches a doc whose literal words say
        // "Saturday" through the doc's indexed canonical terms.
        // Per-query beKIND (Luyi 2026-10-10): location/activity tags are
        // appended to the lexical query via a single daemon round-trip on
        // cache miss only; parsed-query cache hits avoid the daemon entirely.
        // Fail-open is structural (a doc without tags simply has no synthetic
        // terms; a failed daemon call yields no query tags).
        let do_rerank = should_conversational_rerank(
            self.store.options().conversational_rerank,
            session_id,
            query,
        );
        let temporal_context = prepared.temporal_context();
        let freshness_policy = self.store.options().freshness_policy;
        let relative_freshness_enabled = freshness_policy.mode
            == crate::temporal::FreshnessMode::Relative
            && !temporal_context.has_explicit_temporal
            && freshness_policy.weight_for_intent(temporal_context.intent) > 0.0
            && temporal_context
                .freshness_as_of
                .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
                .is_some();
        let depth = if do_rerank || relative_freshness_enabled {
            top_k.max(RERANK_DEEP_TOP_K)
        } else {
            top_k
        };
        let comparison_operands = &prepared.analysis().comparison_operands;
        let is_explicit_comparison = prepared.analysis().intent
            == crate::query_semantics::QueryIntent::Compare
            && comparison_operands.len() == 2;
        let mut lexical = if is_explicit_comparison {
            let mut per_operand = Vec::with_capacity(comparison_operands.len());
            for operand in comparison_operands {
                let operand_prepared = match reference_date {
                    Some(date) => PreparedQuery::new_at_with_rules(
                        operand,
                        date,
                        Arc::clone(&reasoning_rules),
                    ),
                    None => PreparedQuery::new_with_rules(operand, Arc::clone(&reasoning_rules)),
                };
                let mut side_results = match additional_allowed_doc_ids {
                    Some(allowed_doc_ids) => {
                        self.store.query_prepared_cached_with_allowed_doc_ids(
                            &operand_prepared,
                            depth,
                            filters,
                            allowed_doc_ids,
                        )?
                    }
                    None => self
                        .store
                        .query_prepared_cached(&operand_prepared, depth, filters)?,
                };
                if do_rerank {
                    let entities = session_resolved_entities(
                        &self.conversation_states,
                        scope,
                        session_id.unwrap_or(""),
                    );
                    side_results = conversational_rerank(
                        side_results,
                        &self.store,
                        operand,
                        &entities,
                        filters,
                        RERANK_WEIGHTS,
                    );
                    side_results.truncate(depth);
                }
                let user_id = filters.get(USER_FILTER).cloned().unwrap_or_default();
                let structured = if user_id.is_empty() {
                    Vec::new()
                } else {
                    let request = SearchRequest {
                        query: operand.clone(),
                        options: None,
                        user_id,
                        top_k: depth,
                        session_id: session_id.map(String::from),
                        scope: Some(scope.to_string()),
                        filters: None,
                        reference_date: reference_date.map(str::to_owned),
                    };
                    structured_fact_results_with_rules(
                        &self.store,
                        &self.relations_cache,
                        &self.superseded_ids,
                        &request,
                        Arc::clone(&reasoning_rules),
                    )
                };
                side_results = blend_structured_first(structured, side_results, depth);
                for result in &mut side_results {
                    result.comparison_operands.push(operand.clone());
                }
                per_operand.push(side_results);
            }
            interleave_comparison_results(per_operand, top_k)
        } else {
            match additional_allowed_doc_ids {
                Some(allowed_doc_ids) => self.store.query_prepared_cached_with_allowed_doc_ids(
                    &prepared,
                    depth,
                    filters,
                    allowed_doc_ids,
                )?,
                None => self
                    .store
                    .query_prepared_cached(&prepared, depth, filters)?,
            }
        };
        if do_rerank && !is_explicit_comparison {
            let entities = session_resolved_entities(
                &self.conversation_states,
                scope,
                session_id.unwrap_or(""),
            );
            lexical = conversational_rerank(
                lexical,
                &self.store,
                query,
                &entities,
                filters,
                RERANK_WEIGHTS,
            );
            lexical.truncate(if relative_freshness_enabled {
                depth
            } else {
                top_k
            });
        }
        // Structured-fact path: dependency-parse relation evidence blends
        // ahead of the lexical results. The ownership filter comes from the
        // caller's filters when present (benchmark); provider-scoped callers
        // (MCP) get no structured results, which is safe — lexical still works.
        let structured = if is_explicit_comparison {
            Vec::new()
        } else {
            let user_id = filters.get(USER_FILTER).cloned().unwrap_or_default();
            if user_id.is_empty() {
                Vec::new()
            } else {
                let request = SearchRequest {
                    query: query.to_string(),
                    options: None,
                    user_id,
                    top_k: if relative_freshness_enabled {
                        depth
                    } else {
                        top_k
                    },
                    session_id: session_id.map(String::from),
                    scope: Some(scope.to_string()),
                    filters: None,
                    reference_date: reference_date.map(str::to_owned),
                };
                structured_fact_results_with_rules(
                    &self.store,
                    &self.relations_cache,
                    &self.superseded_ids,
                    &request,
                    Arc::clone(&reasoning_rules),
                )
            }
        };
        let results = blend_structured_first(
            structured,
            lexical,
            if relative_freshness_enabled {
                depth
            } else {
                top_k
            },
        );
        // Session tag link expansion (Luyi 2026-10-07): when a doc is boosted
        // via its tags, other docs in the same session sharing those tags
        // get boosted too. Also handles question-type routing ("where"->venue,
        // "when"->temporal). Supplements (not replaces) per-document tag BM25.
        let results = self.apply_session_tag_link_boost(results, session_id, query);
        let mut results = apply_relative_freshness(&self.store, &prepared, results);
        results.truncate(top_k);
        observe_session_search(
            &self.conversation_states,
            scope,
            session_id,
            query,
            &prepared,
            &results,
        );
        Ok(results)
    }
}

/// Lazily-built dependency-parse relation indexes backing the structured-fact
/// retrieval path ("which place did both X and Y visit", "where was X
/// between <dates>").
///
/// One index per user, each composed only from documents visible to that
/// user (ownership, supersession, and expiry are enforced before composition,
/// never after). The index builds on the first structured question in one
/// batched extractor run, and rebuilds when the visible document set changes
/// (fingerprinted by document id, content, and relation-relevant metadata).
/// The write path never pays for it. Extraction is fail-open: a
/// missing/slow/broken extractor leaves the cache empty and callers fall
/// back to lexical retrieval.
#[derive(Default)]
pub(super) struct RelationsCache {
    pub(super) per_user: HashMap<String, UserRelations>,
}

/// Relation index for a single user. `fingerprint` covers the exact visible
/// document set the index was composed from; any content or metadata change
/// invalidates it.
#[derive(Default)]
pub(super) struct UserRelations {
    pub(super) fingerprint: u64,
    pub(super) index: Option<Arc<RelationIndex>>,
}

/// Upper bound on distinct users holding a relation index at once. Eviction
/// only costs a future lazy rebuild, so the bound keeps a multi-user service
/// from growing without limit.
const RELATIONS_CACHE_MAX_USERS: usize = 8;

/// Upper bound for the batched extractor subprocess on the query path.
/// Past this, the query degrades to lexical retrieval rather than stalling.
pub(super) const RELATIONS_EXTRACT_TIMEOUT_SECS: u64 = 120;

/// Fingerprint the exact document set an index was composed from. Every field
/// that feeds `relation_turns_from_docs` (plus the visibility filters) is
/// hashed, so any content or metadata change under an existing id invalidates
/// the cached index.
pub(super) fn relations_fingerprint(docs: &[&SourceDocument]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // Callers pass one user's visible documents sorted by doc id.
    let mut hasher = DefaultHasher::new();
    docs.len().hash(&mut hasher);
    for doc in docs {
        doc.doc_id.hash(&mut hasher);
        doc.content.hash(&mut hasher);
        doc.author_agent.hash(&mut hasher);
        doc.source.hash(&mut hasher);
        doc.group_id.hash(&mut hasher);
        doc.timestamp.hash(&mut hasher);
        // BTreeMap iteration is sorted, so the filter map hashes
        // deterministically; visibility-relevant metadata is part of the
        // fingerprint. The store-generated recorded-at field is deliberately
        // excluded: it does not affect relation extraction or visibility and
        // differs across equivalent store rebuilds.
        for (key, value) in &doc.filters {
            if key == "membrick_recorded_at" {
                continue;
            }
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Build (or reuse) the relation index for one user's visible document set.
/// Returns `None` when extraction fails or times out, so the caller falls
/// through to the lexical path.
fn relations_index_for(
    docs: &[&SourceDocument],
    user_id: &str,
    cache: &Mutex<RelationsCache>,
    python_free: bool,
) -> Option<Arc<RelationIndex>> {
    // The lock is held across the build so concurrent structured queries for
    // the same user share one extractor run instead of racing duplicates.
    let mut cached = cache.lock().expect("relations cache lock poisoned");
    if cached.per_user.len() >= RELATIONS_CACHE_MAX_USERS && !cached.per_user.contains_key(user_id)
    {
        // Bounded: evict an arbitrary entry to make room. Eviction only
        // costs a future lazy rebuild.
        if let Some(key) = cached.per_user.keys().next().cloned() {
            cached.per_user.remove(&key);
        }
    }
    let entry = cached.per_user.entry(user_id.to_string()).or_default();
    let fingerprint = relations_fingerprint(docs);
    if entry.fingerprint == fingerprint {
        return entry.index.clone();
    }
    // Python-free mode: skip the spaCy subprocess entirely. The cache stays
    // empty and callers fail over to the lexical path. A pre-populated
    // cache (tests) is still honored via the fingerprint check above.
    if python_free {
        return None;
    }
    let turns = relation_turns_from_docs(docs);
    // Bound the subprocess: run extraction on a worker thread and give up
    // after the timeout, leaving the cache empty (fail-open to lexical).
    // Detect language for model selection (language PRs added model param).
    let combined_text: String = turns
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let model = crate::lang::default_spacy_model_for_lang(crate::lang::detect_lang(&combined_text))
        .to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let output = extract_relations_via_spacy(
            &turns,
            std::time::Duration::from_secs(RELATIONS_EXTRACT_TIMEOUT_SECS),
            &model,
        );
        let _ = tx.send(output.relations);
    });
    let raw_relations = match rx.recv_timeout(std::time::Duration::from_secs(
        RELATIONS_EXTRACT_TIMEOUT_SECS,
    )) {
        Ok(relations) => relations,
        Err(_) => {
            tracing::warn!(
                "relations: extractor timed out after {RELATIONS_EXTRACT_TIMEOUT_SECS}s; \
                 structured-fact path disabled for this query (not cached, will retry)"
            );
            // Do NOT cache the empty index: a timeout/failure must not
            // poison the cache for unchanged data. Return a transient empty
            // index so this query fails open to lexical, and the next
            // query retries extraction.
            let build_turns = relation_turns_from_docs(docs);
            return Some(Arc::new(RelationIndex::build(&build_turns, &[])));
        }
    };
    let build_turns = relation_turns_from_docs(docs);
    let index = Arc::new(RelationIndex::build(&build_turns, &raw_relations));
    entry.fingerprint = fingerprint;
    entry.index = Some(Arc::clone(&index));
    Some(index)
}

/// Structured-fact retrieval: analyze the question, consult the lazily-built
/// relation index, and return evidence as `SearchResult`s scored above the
/// lexical range. Empty when the option is off, the question is not a
/// structured fact question, or the index holds no evidence.
///
/// Visibility is enforced before composition: the index is built only from
/// this user's live documents (ownership, supersession, expiry), so a
/// foreign, superseded, or expired document can never influence relation
/// composition, even if it would later be filtered from the hits.
#[allow(clippy::too_many_arguments)]
pub(super) fn structured_fact_results(
    store: &IndexStore,
    cache: &Mutex<RelationsCache>,
    superseded_ids: &HashSet<(String, String)>,
    request: &SearchRequest,
) -> Vec<crate::SearchResult> {
    structured_fact_results_with_rules(
        store,
        cache,
        superseded_ids,
        request,
        crate::reasoning_rules::builtin_rules(),
    )
}

fn structured_fact_results_with_rules(
    store: &IndexStore,
    cache: &Mutex<RelationsCache>,
    superseded_ids: &HashSet<(String, String)>,
    request: &SearchRequest,
    reasoning_rules: Arc<crate::reasoning_rules::ReasoningRules>,
) -> Vec<crate::SearchResult> {
    if !store.options().structured_fact_retrieval || request.query.trim().is_empty() {
        return Vec::new();
    }
    // Personal relationships are served from native claims written with each
    // DocRecord. This keeps retrieval in the normal write/query lifecycle.
    if let Some(results) =
        native_relationship_results(store, superseded_ids, request, reasoning_rules)
    {
        if !results.is_empty() {
            return results;
        }
    }
    // Classify before building: only structured fact questions pay for the
    // extractor subprocess. (query_structured re-analyzes; the classifier
    // itself is cheap string matching.)
    if analyze_fact_question(&request.query).is_none() {
        return Vec::new();
    }
    // Enumerating source documents sorts the full corpus. Delay that O(n log
    // n) work until after classification, since ordinary keyword searches
    // have no use for the structured-fact view.
    let docs = store.source_documents();
    let now_ms = unix_time_ms();
    let mut visible: Vec<&SourceDocument> = docs
        .iter()
        .copied()
        .filter(|doc| owns_memory(doc, &request.user_id))
        .filter(|doc| !is_session_observation(doc))
        .filter(|doc| !superseded_ids.contains(&(request.user_id.clone(), doc.doc_id.clone())))
        .filter(|doc| {
            doc.filters
                .get("expires_at_ms")
                .and_then(|value| value.parse::<u64>().ok())
                .is_none_or(|expires_at| expires_at > now_ms)
        })
        .collect();
    visible.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    let index = match relations_index_for(
        &visible,
        &request.user_id,
        cache,
        store.options().python_free(),
    ) {
        Some(index) => index,
        None => return Vec::new(),
    };
    let hits = match query_structured(&index, &request.query) {
        Some(hits) => hits,
        None => return Vec::new(),
    };
    // The index holds only visible documents, so every hit maps; the lookup
    // stays defensive against index/query skew.
    let by_id: HashMap<&str, &SourceDocument> =
        visible.iter().map(|d| (d.doc_id.as_str(), *d)).collect();
    let results = hits
        .into_iter()
        .filter_map(|hit| {
            let doc = by_id.get(hit.doc_id.as_str())?;
            Some(crate::SearchResult {
                doc_id: hit.doc_id,
                source: doc.source.clone(),
                group_id: doc.group_id.clone(),
                score: hit.score,
                score_breakdown: crate::index::ScoreBreakdown::default(),
                matched_entities: Vec::new(),
                matched_terms: Vec::new(),
                probable_topic: None,
                doc_type_guess: None,
                semantic_status: None,
                superseded_by: None,
                relation_confidence: Some(hit.confidence),
                relation_evidence: vec![hit.evidence_label],
                comparison_operands: Vec::new(),
            })
        })
        .collect();
    results
}

/// Return `Some(results)` when native claims support the query's personal
/// relationship, including an empty result when no matching claims exist.
/// `None` means the existing generic structured-fact path may handle it.
fn native_relationship_results(
    store: &IndexStore,
    superseded_ids: &HashSet<(String, String)>,
    request: &SearchRequest,
    reasoning_rules: Arc<crate::reasoning_rules::ReasoningRules>,
) -> Option<Vec<crate::SearchResult>> {
    let prepared = match request.reference_date.as_deref() {
        Some(date) => PreparedQuery::new_at_with_rules(&request.query, date, reasoning_rules),
        None => PreparedQuery::new_with_rules(&request.query, reasoning_rules),
    };
    let relation = prepared.temporal_context().evidence_requirements.relation?;
    let (predicate, is_participation) = match relation {
        QueryEvidenceRelation::PersonalParticipation => ("participated_in", true),
        QueryEvidenceRelation::PersonalAcquisition => ("acquired", false),
    };

    let context = prepared.temporal_context();
    let start = context.starts_from.and_then(crate::parse_reference_date);
    let end = context.ends_at.and_then(crate::parse_reference_date);
    let anchor = context.anchor_date.and_then(crate::parse_reference_date);
    let relative_question = is_participation
        && [
            "relative",
            "family",
            "cousin",
            "sister",
            "brother",
            "aunt",
            "uncle",
            "grandparent",
        ]
        .iter()
        .any(|term| request.query.to_lowercase().contains(term));
    let now_ms = unix_time_ms();
    let mut results = Vec::new();
    let mut event_dates = HashMap::new();
    for record in store.records() {
        let Some(source) = store.source_document_by_id(&record.doc_id) else {
            continue;
        };
        if !owns_memory(source, &request.user_id) {
            continue;
        }
        if is_session_observation(source) {
            continue;
        }
        if superseded_ids.contains(&(request.user_id.clone(), record.doc_id.clone())) {
            continue;
        }
        if source
            .filters
            .get("expires_at_ms")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|expires_at| expires_at <= now_ms)
        {
            continue;
        }
        let claim = record.top_claims.iter().find(|claim| {
            claim.predicate == predicate
                && claim.confidence >= 0.35
                && (is_participation || claim.status == Some(ClaimStatus::Completed))
        });
        let Some(claim) = claim else {
            continue;
        };
        let event_of = is_participation
            .then(|| {
                record
                    .top_claims
                    .iter()
                    .find(|edge| edge.predicate == "event_of" && edge.subject == claim.object)
            })
            .flatten();
        let relative_edge = event_of.and_then(|event_of| {
            record.top_claims.iter().find(|claim| {
                claim.predicate == "relative_of"
                    && claim.subject == event_of.object
                    && claim.object == "user"
            })
        });
        if relative_question && (event_of.is_none() || relative_edge.is_none()) {
            continue;
        }
        let date = record
            .timestamp
            .as_deref()
            .and_then(crate::parse_reference_date);
        if context.hard_filter
            && (date.is_none()
                || start.is_some_and(|start| date.is_some_and(|date| date < start))
                || end.is_some_and(|end| date.is_some_and(|date| date > end)))
        {
            continue;
        }
        event_dates.insert(record.doc_id.clone(), date);
        let role = is_participation
            .then(|| {
                record
                    .top_claims
                    .iter()
                    .find(|edge| edge.predicate == "participant_role" && edge.subject == "user")
            })
            .flatten();
        let event_type = is_participation
            .then(|| {
                record
                    .top_claims
                    .iter()
                    .find(|edge| edge.predicate == "event_type" && edge.subject == claim.object)
            })
            .flatten();
        let event_time = is_participation
            .then(|| {
                record
                    .top_claims
                    .iter()
                    .find(|edge| edge.predicate == "event_time" && edge.subject == claim.object)
            })
            .flatten();
        let mut evidence = format!("user {} {}", predicate, claim.object);
        if let Some(role) = role {
            evidence.push_str(&format!("; role: {}", role.object));
        }
        if let Some(event_type) = event_type {
            evidence.push_str(&format!("; event_type: {}", event_type.object));
        }
        if let Some(event_of) = event_of {
            evidence.push_str(&format!("; event_of: {}", event_of.object));
        }
        if let Some(relative_edge) = relative_edge {
            evidence.push_str(&format!(
                "; {} relative_of {}",
                relative_edge.subject, relative_edge.object
            ));
        }
        if let Some(event_time) = event_time {
            evidence.push_str(&format!("; event_time: {}", event_time.object));
        }
        let recency = match (date, anchor) {
            (Some(date), Some(anchor)) => {
                5.0 / (1.0 + date.signed_duration_since(anchor).num_days().unsigned_abs() as f32)
            }
            _ => 0.0,
        };
        results.push(crate::SearchResult {
            doc_id: record.doc_id.clone(),
            source: record.source.clone(),
            group_id: record.group_id.clone(),
            score: 100.0 + claim.confidence * 10.0 + recency,
            score_breakdown: crate::index::ScoreBreakdown::default(),
            matched_entities: vec![claim.object.clone()],
            matched_terms: Vec::new(),
            probable_topic: record.probable_topic.clone(),
            doc_type_guess: record.doc_type_guess.clone(),
            semantic_status: None,
            superseded_by: None,
            relation_confidence: Some(claim.confidence),
            relation_evidence: vec![evidence],
            comparison_operands: Vec::new(),
        });
    }
    let chronological =
        context.evidence_requirements.order == Some(QueryEvidenceOrder::EventDateAscending);
    results.sort_by(|a, b| {
        let chronological_order = if chronological {
            match (
                event_dates.get(&a.doc_id).copied().flatten(),
                event_dates.get(&b.doc_id).copied().flatten(),
            ) {
                (Some(a_date), Some(b_date)) => a_date.cmp(&b_date),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        } else {
            std::cmp::Ordering::Equal
        };
        chronological_order
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    Some(results)
}

/// Blend structured-fact evidence ahead of lexical results, deduplicating
/// by document id and respecting `top_k`.
pub(super) fn blend_structured_first(
    structured: Vec<crate::SearchResult>,
    lexical: Vec<crate::SearchResult>,
    top_k: usize,
) -> Vec<crate::SearchResult> {
    if structured.is_empty() {
        return lexical;
    }
    let seen: HashSet<String> = structured.iter().map(|r| r.doc_id.clone()).collect();
    let mut blended = structured;
    let room = top_k.saturating_sub(blended.len());
    blended.extend(
        lexical
            .into_iter()
            .filter(|r| !seen.contains(r.doc_id.as_str()))
            .take(room),
    );
    blended
}

/// Apply relative freshness after MemoryService has merged lexical and
/// structured results and completed its other rerankers. Structured facts
/// retain their established priority tier; freshness only reorders results
/// within each tier.
fn apply_relative_freshness(
    store: &IndexStore,
    prepared: &PreparedQuery,
    mut results: Vec<crate::SearchResult>,
) -> Vec<crate::SearchResult> {
    let policy = store.options().freshness_policy;
    let context = prepared.temporal_context();
    let weight = policy.weight_for_intent(context.intent);
    if policy.mode != crate::temporal::FreshnessMode::Relative
        || context.has_explicit_temporal
        || weight <= 0.0
    {
        return results;
    }
    let Some(as_of) = context
        .freshness_as_of
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| Utc.from_utc_datetime(&date))
    else {
        return results;
    };

    for result in &mut results {
        let timestamp = store
            .record_by_id(&result.doc_id)
            .and_then(|record| record.timestamp.as_deref());
        if let Some(delta) = crate::temporal::relative_freshness_delta(
            result.score,
            timestamp,
            as_of,
            policy.half_life_days,
            weight,
        ) {
            result.score += delta;
            result.score_breakdown.recency_score += delta;
        }
    }

    sort_by_adjusted_score_with_structured_priority(results)
}

pub(super) fn sort_by_adjusted_score_with_structured_priority(
    results: Vec<crate::SearchResult>,
) -> Vec<crate::SearchResult> {
    let mut structured = Vec::new();
    let mut lexical = Vec::new();
    for result in results {
        if result.relation_evidence.is_empty() {
            lexical.push(result);
        } else {
            structured.push(result);
        }
    }
    // `sort_by` is stable: equal scores retain the preceding retrieval order.
    // A secondary doc-ID ordering would reshuffle ties even at weight zero.
    let by_adjusted_score = |a: &crate::SearchResult, b: &crate::SearchResult| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    structured.sort_by(by_adjusted_score);
    lexical.sort_by(by_adjusted_score);
    structured.extend(lexical);
    structured
}

/// Merge independently ranked comparison sides while keeping both sides
/// represented in the result window. A document returned for both operands
/// appears once and retains both labels.
pub(super) fn interleave_comparison_results(
    sides: Vec<Vec<crate::SearchResult>>,
    top_k: usize,
) -> Vec<crate::SearchResult> {
    let mut sides = sides.into_iter().map(VecDeque::from).collect::<Vec<_>>();
    let mut merged = Vec::<crate::SearchResult>::new();
    let mut positions = HashMap::<String, usize>::new();
    while merged.len() < top_k && sides.iter().any(|side| !side.is_empty()) {
        for side in &mut sides {
            let Some(mut result) = side.pop_front() else {
                continue;
            };
            if let Some(index) = positions.get(&result.doc_id).copied() {
                let existing = &mut merged[index];
                for operand in result.comparison_operands.drain(..) {
                    if !existing.comparison_operands.contains(&operand) {
                        existing.comparison_operands.push(operand);
                    }
                }
                for entity in result.matched_entities.drain(..) {
                    if !existing.matched_entities.contains(&entity) {
                        existing.matched_entities.push(entity);
                    }
                }
                for term in result.matched_terms.drain(..) {
                    if !existing.matched_terms.contains(&term) {
                        existing.matched_terms.push(term);
                    }
                }
                existing.score = existing.score.max(result.score);
                continue;
            }
            positions.insert(result.doc_id.clone(), merged.len());
            merged.push(result);
            if merged.len() == top_k {
                break;
            }
        }
    }
    merged
}
