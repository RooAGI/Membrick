//! Search review: MCP search history, review pages, and search feedback.

use super::*;

impl MemoryService {
    pub(crate) fn start_search_history_index(&self, project_root: &Path) {
        let _ = self.search_history_index.ensure_worker(project_root);
    }

    pub(super) fn search_review_entry(
        &self,
        project_root: &Path,
        search_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let mut state = self
            .search_review_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Ok(state.load_project(project_root)?.get(search_id).cloned())
    }

    pub(super) fn set_search_review_entry(
        &self,
        project_root: &Path,
        search_id: &str,
        entry: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut state = self
            .search_review_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let reviews = state.load_project(project_root)?;
        let previous = reviews.insert(search_id.to_string(), entry);
        let path = project_root
            .join(".membrick")
            .join("search-review-state.json");
        std::fs::create_dir_all(path.parent().expect("review state path has parent"))?;
        let persisted = serde_json::json!({
            "schema_version": 1,
            "reviews": reviews,
        });
        if let Err(error) = write_search_review_state(&path, &persisted) {
            if let Some(previous) = previous {
                reviews.insert(search_id.to_string(), previous);
            } else {
                reviews.remove(search_id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn lookup_search_history_record(
        &self,
        project_root: &Path,
        search_id: &str,
    ) -> anyhow::Result<SearchHistoryLookup> {
        let history_path = project_root
            .join(".membrick")
            .join("mcp-call-history.jsonl");
        let state = self.search_history_index.ensure_worker(project_root);
        let (
            generation,
            location,
            ambiguous,
            malformed_records,
            oversized_records,
            first_malformed_offset,
            ready,
            scanned,
            captured,
            error,
        ) = {
            let state = state.lock().unwrap_or_else(|poison| poison.into_inner());
            (
                state.generation.clone(),
                state.locations.get(search_id).copied(),
                state.ambiguous_ids.contains(search_id),
                state.malformed_records,
                state.oversized_records,
                state.first_malformed_offset,
                state.initialized
                    && state.scanned_offset >= state.captured_end
                    && state.partial_line.is_empty()
                    && !state.partial_oversized
                    && state.last_error.is_none(),
                state.scanned_offset,
                state.captured_end,
                state.last_error.clone(),
            )
        };
        let (current_generation, latest_len) = search_history_file_identity(&history_path)?;
        let ready = ready
            && latest_len == captured
            && generation.as_deref() == Some(current_generation.as_str());
        if ambiguous {
            anyhow::bail!("search_id is ambiguous in recorded MCP history")
        }
        if let (Some(generation), Some(location)) = (generation.as_deref(), location) {
            match read_search_history_location(&history_path, generation, location, search_id) {
                Ok(record) => return Ok(SearchHistoryLookup::Found(record)),
                Err(_) => {
                    let state = self.search_history_index.state_for(project_root);
                    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
                    state.generation = None;
                    state.initialized = false;
                    return Ok(SearchHistoryLookup::Pending(serde_json::json!({
                        "status": "indexing",
                        "search_id": search_id,
                        "indexed_bytes": scanned,
                        "history_bytes": latest_len.max(captured),
                        "message": "Search history changed; the resident is rebuilding the lookup index. Retry shortly."
                    })));
                }
            }
        }
        if ready {
            if malformed_records > 0 || oversized_records > 0 {
                return Ok(SearchHistoryLookup::Incomplete {
                    malformed_records,
                    oversized_records,
                    first_malformed_offset,
                });
            }
            return Ok(SearchHistoryLookup::Absent);
        }
        Ok(SearchHistoryLookup::Pending(serde_json::json!({
            "status": "indexing",
            "search_id": search_id,
            "indexed_bytes": scanned,
            "history_bytes": captured,
            "message": error.unwrap_or_else(|| "The resident is rebuilding the search history lookup index. Retry shortly.".to_string())
        })))
    }

    /// Return either a page of unreviewed MCP searches or one exact search.
    /// Review state is keyed by the stable MCP trace ID and is owned by this
    /// project service, while the append-only telemetry ledger remains intact.
    pub(crate) fn review_search_mcp(
        &self,
        project_root: &Path,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let args = arguments
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("review_search arguments must be an object"))?;
        if let Some(unknown) = args
            .keys()
            .find(|key| !["search_id", "cursor", "page_size"].contains(&key.as_str()))
        {
            anyhow::bail!("unknown review_search argument: {unknown}");
        }

        let history_path = project_root
            .join(".membrick")
            .join("mcp-call-history.jsonl");
        if let Some(search_id) = args.get("search_id").and_then(serde_json::Value::as_str) {
            anyhow::ensure!(!search_id.trim().is_empty(), "search_id must not be blank");
            let prior_feedback = self.search_review_entry(project_root, search_id)?;
            let record = match self.lookup_search_history_record(project_root, search_id)? {
                SearchHistoryLookup::Found(record) => record,
                SearchHistoryLookup::Pending(progress) => return Ok(progress),
                SearchHistoryLookup::Incomplete {
                    malformed_records,
                    oversized_records,
                    first_malformed_offset,
                } => {
                    return Ok(serde_json::json!({
                        "status": "history_incomplete",
                        "search_id": search_id,
                        "malformed_records_skipped": malformed_records,
                        "oversized_records_skipped": oversized_records,
                        "first_malformed_offset": first_malformed_offset,
                        "message": "The search ID was not found, but malformed or oversized history records mean its absence cannot be confirmed."
                    }));
                }
                SearchHistoryLookup::Absent => anyhow::bail!(
                    "search_id was not found in recorded MCP history after the resident caught up"
                ),
            };
            let result = search_result_payload(&record);
            let documents = self.documents_for_review(&result)?;
            let response = serde_json::json!({
                "search_id": search_id,
                "reviewed": prior_feedback.is_some(),
                "search": record,
                "result": result,
                "documents": documents,
                "prior_feedback": prior_feedback.as_ref().and_then(|v| v.get("feedback")),
            });
            anyhow::ensure!(
                serde_json::to_vec(&response)?.len() <= MAX_REVIEW_PAGE_RESPONSE_BYTES,
                "search review exceeds the response byte limit; the recorded result is too large to return safely"
            );
            return Ok(response);
        }

        let page_size = args
            .get("page_size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 100) as usize;
        let state = self.search_history_index.ensure_worker(project_root);
        let (
            generation,
            captured_end,
            indexed_end,
            ready,
            oversized,
            malformed,
            first_malformed_offset,
            last_error,
        ) = {
            let state = state.lock().unwrap_or_else(|poison| poison.into_inner());
            (
                state.generation.clone(),
                state.captured_end,
                state.scanned_offset,
                state.initialized
                    && state.scanned_offset >= state.captured_end
                    && state.partial_line.is_empty()
                    && !state.partial_oversized
                    && state.last_error.is_none(),
                state.oversized_records,
                state.malformed_records,
                state.first_malformed_offset,
                state.last_error.clone(),
            )
        };
        let (start_offset, snapshot_end) =
            match args.get("cursor").and_then(serde_json::Value::as_str) {
                Some(value) => parse_search_history_cursor(&history_path, value)
                    .map_err(|error| anyhow::anyhow!("invalid continuation cursor: {error}"))?,
                None => (0, captured_end),
            };
        let (current_generation, current_history_end) =
            search_history_file_identity(&history_path)?;
        let ready = ready
            && current_history_end == captured_end
            && generation.as_deref() == Some(current_generation.as_str());
        if !ready {
            return Ok(serde_json::json!({
                "status": "indexing",
                "searches": [],
                "indexed_bytes": indexed_end,
                "history_bytes": current_history_end.max(captured_end),
                "message": last_error.unwrap_or_else(|| "The resident is rebuilding the search history lookup index. Retry shortly.".to_string())
            }));
        }
        let generation = match generation {
            Some(generation) => generation,
            None => {
                return Ok(serde_json::json!({
                    "searches": [],
                    "next_cursor": null,
                    "has_more": false,
                    "oversized_records_skipped": 0,
                    "malformed_records_skipped": 0,
                    "first_malformed_offset": null,
                    "ambiguous_searches_skipped": 0,
                }));
            }
        };
        anyhow::ensure!(
            snapshot_end <= captured_end,
            "search history index has not caught up to this cursor; retry shortly"
        );
        let candidates = {
            let state = state.lock().unwrap_or_else(|poison| poison.into_inner());
            let start_index = state
                .ordered_searches
                .partition_point(|(offset, _)| *offset < start_offset);
            state
                .ordered_searches
                .iter()
                .skip(start_index)
                .take(MAX_REVIEW_PAGE_RECORDS_SCANNED + 1)
                .take_while(|(offset, _)| *offset < snapshot_end)
                .map(|(offset, search_id)| {
                    (
                        *offset,
                        search_id.clone(),
                        state.locations.get(search_id).copied(),
                        state.ambiguous_ids.contains(search_id),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut page = Vec::new();
        let mut page_history_bytes = 0usize;
        let mut page_response_bytes = 0usize;
        let mut position = start_offset;
        let mut has_more = false;
        let mut ambiguous_searches_skipped = 0usize;
        for (candidate_index, (offset, search_id, location, ambiguous)) in
            candidates.iter().enumerate()
        {
            if candidate_index == MAX_REVIEW_PAGE_RECORDS_SCANNED {
                position = *offset;
                has_more = true;
                break;
            }
            if page.len() >= page_size {
                position = *offset;
                has_more = true;
                break;
            }
            let Some(location) = *location else {
                if *ambiguous {
                    ambiguous_searches_skipped += 1;
                }
                continue;
            };
            position = location.offset.saturating_add(location.length);
            if *ambiguous {
                ambiguous_searches_skipped += 1;
                continue;
            }
            if self.search_review_entry(project_root, search_id)?.is_some() {
                continue;
            }
            if page.len() >= page_size
                || page_history_bytes.saturating_add(location.length as usize)
                    > MAX_REVIEW_PAGE_HISTORY_BYTES
            {
                anyhow::ensure!(
                    !page.is_empty(),
                    "one search record exceeds the review page byte limit; use its search_id for exact review"
                );
                position = *offset;
                has_more = true;
                break;
            }
            let record =
                read_search_history_location(&history_path, &generation, location, search_id)?;
            let result = search_result_payload(&record);
            let item = serde_json::json!({
                "search_id": search_id,
                "search": record,
                "result": result,
                "documents": self.documents_for_review(&result)?,
            });
            let item_bytes = serde_json::to_vec(&item)?.len();
            if page_response_bytes.saturating_add(item_bytes) > MAX_REVIEW_PAGE_RESPONSE_BYTES {
                anyhow::ensure!(
                    !page.is_empty(),
                    "one search review exceeds the response byte limit; use its search_id for exact review"
                );
                position = *offset;
                has_more = true;
                break;
            }
            page.push(item);
            page_history_bytes = page_history_bytes.saturating_add(location.length as usize);
            page_response_bytes = page_response_bytes.saturating_add(item_bytes);
        }

        let next_cursor = if has_more {
            Some(search_history_cursor(
                &history_path,
                &generation,
                position,
                snapshot_end,
            )?)
        } else {
            None
        };
        Ok(serde_json::json!({
            "searches": page,
            "next_cursor": next_cursor,
            "has_more": has_more,
            "oversized_records_skipped": oversized,
            "malformed_records_skipped": malformed,
            "first_malformed_offset": first_malformed_offset,
            "ambiguous_searches_skipped": ambiguous_searches_skipped,
        }))
    }

    /// Return one page from the latest offline dashboard snapshot. This reads
    /// neither source history nor review state and does not need a service
    /// instance or resident lock.
    pub fn dashboard_search_feedback_reviews(
        project_root: &Path,
        offset: usize,
        page_size: usize,
    ) -> anyhow::Result<serde_json::Value> {
        let directory = project_root.join(".membrick");
        let snapshot_path = directory.join("search-feedback-dashboard.jsonl");
        let file = match std::fs::File::open(&snapshot_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(serde_json::json!({
                    "reviews": [],
                    "evaluation": null,
                    "total": 0,
                    "offset": 0,
                    "page_size": page_size.clamp(1, 100),
                    "next_offset": null,
                    "evaluation_status": "not_run",
                }));
            }
            Err(error) => return Err(error.into()),
        };
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        let Some((oversized, terminated)) =
            read_bounded_history_line(&mut reader, &mut line, MAX_DASHBOARD_ROW_BYTES)?
        else {
            anyhow::bail!("offline search dashboard snapshot is empty");
        };
        anyhow::ensure!(
            terminated,
            "offline search dashboard header is unterminated"
        );
        anyhow::ensure!(
            !oversized,
            "offline search dashboard header exceeds the 512 KiB limit"
        );
        let header: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1])?;
        let report = header
            .get("_snapshot")
            .ok_or_else(|| anyhow::anyhow!("offline search dashboard snapshot has no header"))?;
        anyhow::ensure!(
            report
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                == Some(1),
            "unsupported offline search evaluation report"
        );
        let total = report
            .get("reviewed_searches")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("offline search dashboard header has no row count"))?;
        let total = usize::try_from(total)
            .map_err(|_| anyhow::anyhow!("offline search dashboard row count is too large"))?;
        let page_size = page_size.clamp(1, 100);
        let offset = offset.min(total);
        let mut page = Vec::with_capacity(page_size);
        let mut row_index = 0usize;
        while row_index < total && page.len() < page_size {
            let Some((oversized, terminated)) =
                read_bounded_history_line(&mut reader, &mut line, MAX_DASHBOARD_ROW_BYTES)?
            else {
                anyhow::bail!("offline search dashboard has fewer rows than its header declares");
            };
            anyhow::ensure!(
                terminated,
                "offline search dashboard has an unterminated row"
            );
            anyhow::ensure!(
                !oversized,
                "offline search dashboard row exceeds the 512 KiB limit"
            );
            if row_index >= offset {
                page.push(serde_json::from_slice::<serde_json::Value>(
                    &line[..line.len().saturating_sub(1)],
                )?);
            }
            row_index += 1;
        }
        let next_offset = (offset + page.len() < total).then_some(offset + page.len());
        Ok(serde_json::json!({
            "reviews": page,
            "evaluation": report.get("evaluation"),
            "evaluation_generated_at": report.get("generated_at"),
            "evaluation_status": "ready",
            "total": total,
            "offset": offset,
            "page_size": page_size,
            "next_offset": next_offset,
        }))
    }

    fn source_documents_for_review<'a>(
        &'a self,
        result: &serde_json::Value,
    ) -> anyhow::Result<Vec<&'a SourceDocument>> {
        let mut wanted = Vec::<String>::new();
        let mut seen = HashSet::<String>::new();
        if let Some(hits) = result.get("results").and_then(serde_json::Value::as_array) {
            for hit in hits {
                if let Some(id) = hit
                    .get("id")
                    .or_else(|| hit.get("doc_id"))
                    .and_then(serde_json::Value::as_str)
                {
                    if seen.insert(id.to_string()) {
                        wanted.push(id.to_string());
                    }
                }
            }
        }
        wanted
            .into_iter()
            .filter_map(|doc_id| {
                let captured = self.store.source_document_by_id(&doc_id);
                let workspace = self
                    .workspace_store
                    .as_deref()
                    .and_then(|store| store.store.source_document_by_id(&doc_id));
                Some((doc_id, captured, workspace))
            })
            .filter_map(|(doc_id, captured, workspace)| {
                match (captured, workspace) {
                    (Some(document), None) | (None, Some(document)) => Some(Ok(document)),
                    (None, None) => None,
                    (Some(_), Some(_)) => Some(Err(anyhow::anyhow!(
                        "review result document ID is ambiguous across MemoryService stores: {doc_id}"
                    ))),
                }
            })
            .map(|document| document)
            .collect()
    }

    fn documents_for_review(
        &self,
        result: &serde_json::Value,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        self.source_documents_for_review(result)?
            .into_iter()
            .map(|document| {
                let source_hash = crate::source::source_document_hash(document)?;
                let content_bytes = document.content.len();
                let mut preview_end = content_bytes.min(MAX_REVIEW_DOCUMENT_CONTENT_BYTES);
                while !document.content.is_char_boundary(preview_end) {
                    preview_end -= 1;
                }
                let content_truncated = preview_end < content_bytes;
                let preview = ReviewSourceDocument {
                    doc_id: &document.doc_id,
                    source: &document.source,
                    content: &document.content[..preview_end],
                    concept: &document.concept,
                    group_id: &document.group_id,
                    headings: &document.headings,
                    links: &document.links,
                    timestamp: &document.timestamp,
                    doc_length: document.doc_length,
                    author_agent: &document.author_agent,
                    filters: &document.filters,
                    key_phrases: &document.key_phrases,
                    key_phrase_extraction_hash: &document.key_phrase_extraction_hash,
                };
                Ok(serde_json::json!({
                    "doc_id": document.doc_id,
                    "source_hash": source_hash,
                    "content_bytes": content_bytes,
                    "content_truncated": content_truncated,
                    "document": preview,
                }))
            })
            .collect()
    }

    /// Store one search's usefulness label and explanation. Document
    /// enrichment is handled separately by `enrich_doc`.
    pub(crate) fn search_feedback_mcp(
        &mut self,
        project_root: &Path,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let args = arguments
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("search_feedback arguments must be an object"))?;
        if let Some(unknown) = args.keys().find(|key| {
            ![
                "search_id",
                "label",
                "feedback",
                "document_judgments",
                "query_rule_additions",
            ]
            .contains(&key.as_str())
        }) {
            anyhow::bail!("unknown search_feedback argument: {unknown}");
        }
        let search_id = args
            .get("search_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("search_id is required"))?;
        let label = args
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("label is required"))?;
        anyhow::ensure!(
            matches!(label, "helpful" | "partially_helpful" | "not_helpful"),
            "label must be helpful, partially_helpful, or not_helpful"
        );
        let feedback = args
            .get("feedback")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("feedback must explain the search outcome"))?;
        let query_rule_additions: BTreeMap<String, Vec<String>> = args
            .get("query_rule_additions")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();

        let record = match self.lookup_search_history_record(project_root, search_id)? {
            SearchHistoryLookup::Found(record) => record,
            SearchHistoryLookup::Pending(progress) => anyhow::bail!(
                "search history index is still building ({} of {} bytes indexed); retry shortly",
                progress["indexed_bytes"].as_u64().unwrap_or(0),
                progress["history_bytes"].as_u64().unwrap_or(0)
            ),
            SearchHistoryLookup::Incomplete {
                malformed_records,
                oversized_records,
                first_malformed_offset,
            } => anyhow::bail!(
                "search history is incomplete: skipped {malformed_records} malformed and {oversized_records} oversized records; first malformed record offset is {first_malformed_offset:?}; search absence cannot be confirmed"
            ),
            SearchHistoryLookup::Absent => anyhow::bail!(
                "search_id was not found in recorded MCP history after the resident caught up"
            ),
        };
        let result = search_result_payload(&record);
        let result_hits = result
            .get("results")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("recorded search has no results array"))?;
        let supplied_judgments = args
            .get("document_judgments")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("document_judgments array is required"))?;
        let mut supplied_by_id = HashMap::new();
        for judgment in supplied_judgments {
            let judgment = judgment
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("each document judgment must be an object"))?;
            if let Some(unknown) = judgment
                .keys()
                .find(|key| !["doc_id", "relevance", "feedback"].contains(&key.as_str()))
            {
                anyhow::bail!("unknown document judgment argument: {unknown}");
            }
            let doc_id = judgment
                .get("doc_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("document judgment doc_id is required"))?;
            let relevance = judgment
                .get("relevance")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| {
                    matches!(*value, "relevant" | "partially_relevant" | "not_relevant")
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "document relevance must be relevant, partially_relevant, or not_relevant"
                    )
                })?;
            let feedback = judgment
                .get("feedback")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            anyhow::ensure!(
                supplied_by_id
                    .insert(
                        doc_id.to_string(),
                        (relevance.to_string(), feedback.map(str::to_string))
                    )
                    .is_none(),
                "duplicate document judgment for {doc_id}"
            );
        }
        let mut normalized_judgments = Vec::with_capacity(result_hits.len());
        for (rank, hit) in result_hits.iter().enumerate() {
            let doc_id = hit
                .get("doc_id")
                .or_else(|| hit.get("id"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("recorded result at rank {} has no doc_id", rank + 1)
                })?;
            let (relevance, feedback) = supplied_by_id.remove(doc_id).ok_or_else(|| {
                anyhow::anyhow!("missing document judgment for returned document {doc_id}")
            })?;
            normalized_judgments.push(serde_json::json!({
                "doc_id": doc_id,
                "rank": rank + 1,
                "source_hash": hit.get("source_hash").cloned().unwrap_or(serde_json::Value::Null),
                "relevance": relevance,
                "feedback": feedback,
            }));
        }
        anyhow::ensure!(
            supplied_by_id.is_empty(),
            "document judgments contain IDs not returned by this search: {}",
            supplied_by_id
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        let review = serde_json::json!({
            "search_id": search_id,
            "label": label,
            "feedback": feedback,
            "document_judgments": normalized_judgments,
        });
        let request_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&review, &query_rule_additions))?)
        );
        let prior_entry = self.search_review_entry(project_root, search_id)?;
        let retrying_pending = if let Some(prior) = prior_entry.as_ref() {
            anyhow::ensure!(
                prior
                    .get("request_hash")
                    .and_then(serde_json::Value::as_str)
                    == Some(request_hash.as_str()),
                "search_id already has different feedback"
            );
            if let Some(receipt) = prior.get("receipt") {
                return Ok(receipt.clone());
            }
            true
        } else {
            false
        };
        let mut review_entry = prior_entry.unwrap_or(serde_json::Value::Null);

        if !query_rule_additions.is_empty() {
            let context_documents = self.source_documents_for_review(&result)?;
            let query = record
                .arguments
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_lowercase();
            for terms in query_rule_additions.values() {
                for term in terms {
                    let term = term.trim();
                    anyhow::ensure!(!term.is_empty(), "query rule additions cannot be empty");
                    anyhow::ensure!(
                        query.contains(&term.to_lowercase())
                            || context_documents
                                .iter()
                                .any(|document| source_document_contains_term(document, term)),
                        "query rule addition {term:?} must appear in the reviewed query or source documents"
                    );
                }
            }
            let rules = self.project_rules.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "query rule additions require the workspace MemoryService rules configuration"
                )
            })?;
            rules.validate_query_rule_terms(&query_rule_additions)?;
        }

        if !retrying_pending {
            review_entry = serde_json::json!({
                "request_hash": request_hash,
                "feedback": review,
                "status": "pending",
                "submitted_at_ms": unix_time_ms(),
            });
            self.set_search_review_entry(project_root, search_id, review_entry.clone())?;
        }
        let accepted_query_rules = if query_rule_additions.is_empty() {
            0
        } else {
            self.project_rules
                .as_deref()
                .expect("query rules validated")
                .add_query_rule_terms(query_rule_additions)?
        };
        let receipt = serde_json::json!({
            "search_id": search_id,
            "label": label,
            "status": "reviewed",
            "judged_documents": normalized_judgments.len(),
            "changed_documents": 0,
            "changed_document_ids": [],
            "accepted_query_rules": accepted_query_rules,
            "publication_status": "not_required",
        });
        review_entry["status"] = serde_json::json!("reviewed");
        review_entry["receipt"] = receipt.clone();
        self.set_search_review_entry(project_root, search_id, review_entry)?;
        Ok(receipt)
    }
}

pub(super) fn read_search_review_state(path: &Path) -> anyhow::Result<serde_json::Value> {
    let state = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({"schema_version":1,"reviews":{}})
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        state
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
            && state
                .get("reviews")
                .is_some_and(serde_json::Value::is_object),
        "unsupported or invalid search review state"
    );
    Ok(state)
}

pub(super) fn write_search_review_state(
    path: &Path,
    state: &serde_json::Value,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut temp = path.to_path_buf();
    temp.set_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&serde_json::to_vec(state)?)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

#[derive(Default)]
pub(super) struct SearchReviewStateIndex {
    /// Per-project feedback snapshots loaded once by the resident. The JSON
    /// file remains the durable copy and is atomically rewritten on updates.
    projects: HashMap<PathBuf, HashMap<String, serde_json::Value>>,
}

impl SearchReviewStateIndex {
    fn load_project(
        &mut self,
        project_root: &Path,
    ) -> anyhow::Result<&mut HashMap<String, serde_json::Value>> {
        let project_root = project_root.to_path_buf();
        if !self.projects.contains_key(&project_root) {
            let path = project_root
                .join(".membrick")
                .join("search-review-state.json");
            let state = read_search_review_state(&path)?;
            let reviews = state["reviews"]
                .as_object()
                .expect("validated search review state contains reviews")
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect();
            self.projects.insert(project_root.clone(), reviews);
        }
        Ok(self
            .projects
            .get_mut(&project_root)
            .expect("search review state was loaded"))
    }
}

pub(super) fn parse_mcp_search_history(
    bytes: &[u8],
) -> anyhow::Result<Vec<(usize, crate::telemetry::McpCallRecord)>> {
    let mut records = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let Some(relative_end) = bytes[position..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let end = position + relative_end + 1;
        let record =
            serde_json::from_slice::<crate::telemetry::McpCallRecord>(&bytes[position..end - 1]);
        if let Ok(record) = record {
            if record.tool == "search" {
                records.push((position, record));
            }
        }
        position = end;
    }
    Ok(records)
}

pub(super) const MAX_REVIEW_HISTORY_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_REVIEW_DOCUMENT_CONTENT_BYTES: usize = 8 * 1024;
const MAX_REVIEW_PAGE_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_REVIEW_PAGE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const SEARCH_HISTORY_INDEX_BATCH_BYTES: usize = 1024 * 1024;
const SEARCH_HISTORY_INDEX_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_REVIEW_PAGE_RECORDS_SCANNED: usize = 500;

#[derive(Debug, Clone, Copy)]
struct SearchHistoryLocation {
    offset: u64,
    length: u64,
}

#[derive(Default)]
pub(super) struct SearchHistoryIndexState {
    pub(super) initialized: bool,
    pub(super) generation: Option<String>,
    pub(super) scanned_offset: u64,
    pub(super) captured_end: u64,
    partial_start: u64,
    pub(super) partial_line: Vec<u8>,
    partial_oversized: bool,
    locations: HashMap<String, SearchHistoryLocation>,
    ordered_searches: Vec<(u64, String)>,
    oversized_records: usize,
    malformed_records: usize,
    first_malformed_offset: Option<u64>,
    ambiguous_ids: HashSet<String>,
    pub(super) worker_running: bool,
    last_error: Option<String>,
}

#[derive(Default)]
pub(super) struct SearchHistoryIndex {
    roots: Mutex<HashMap<PathBuf, Arc<Mutex<SearchHistoryIndexState>>>>,
}

impl SearchHistoryIndex {
    pub(super) fn state_for(&self, root: &Path) -> Arc<Mutex<SearchHistoryIndexState>> {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Arc::clone(
            roots
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(SearchHistoryIndexState::default()))),
        )
    }

    fn ensure_worker(self: &Arc<Self>, root: &Path) -> Arc<Mutex<SearchHistoryIndexState>> {
        let state = self.state_for(root);
        let should_start = {
            let mut state_guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
            if state_guard.worker_running {
                false
            } else {
                state_guard.worker_running = true;
                true
            }
        };
        if should_start {
            let weak_index = Arc::downgrade(self);
            let history_path = root.join(".membrick").join("mcp-call-history.jsonl");
            let worker_state = Arc::clone(&state);
            let spawn = std::thread::Builder::new()
                .name("search-history-index".into())
                .spawn(move || {
                    search_history_index_worker(weak_index, history_path, worker_state);
                });
            if let Err(error) = spawn {
                let mut state_guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
                state_guard.worker_running = false;
                state_guard.last_error =
                    Some(format!("failed to start search history index: {error}"));
            }
        }
        state
    }
}

#[cfg(unix)]
pub(super) fn search_history_generation(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}-{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
pub(super) fn search_history_generation(metadata: &std::fs::Metadata) -> String {
    format!("{:?}", metadata.created().ok())
}

fn search_history_file_identity(path: &Path) -> anyhow::Result<(String, u64)> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok((search_history_generation(&metadata), metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(("missing".to_string(), 0))
        }
        Err(error) => Err(error.into()),
    }
}

fn reset_search_history_index(state: &mut SearchHistoryIndexState, generation: String) {
    state.initialized = false;
    state.generation = Some(generation);
    state.scanned_offset = 0;
    state.captured_end = 0;
    state.partial_start = 0;
    state.partial_line.clear();
    state.partial_oversized = false;
    state.locations.clear();
    state.ordered_searches.clear();
    state.oversized_records = 0;
    state.malformed_records = 0;
    state.first_malformed_offset = None;
    state.ambiguous_ids.clear();
    state.last_error = None;
}

fn search_history_index_worker(
    weak_index: std::sync::Weak<SearchHistoryIndex>,
    history_path: PathBuf,
    state: Arc<Mutex<SearchHistoryIndexState>>,
) {
    loop {
        if weak_index.upgrade().is_none() {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            let mut file = match std::fs::File::open(&history_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let mut state_guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
                    reset_search_history_index(&mut state_guard, "missing".to_string());
                    state_guard.initialized = true;
                    state_guard.last_error = None;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            let generation = search_history_generation(&metadata);
            let end = metadata.len();
            use std::io::{Read, Seek, SeekFrom};
            let mut state_guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
            if state_guard.generation.as_deref() != Some(generation.as_str())
                || end < state_guard.scanned_offset
            {
                reset_search_history_index(&mut state_guard, generation);
            }
            state_guard.initialized = true;
            state_guard.captured_end = end;
            state_guard.last_error = None;
            let remaining = end.saturating_sub(state_guard.scanned_offset);
            if remaining == 0 {
                return Ok(());
            }
            let take = remaining.min(SEARCH_HISTORY_INDEX_BATCH_BYTES as u64) as usize;
            let batch_start = state_guard.scanned_offset;
            file.seek(SeekFrom::Start(batch_start))?;
            let mut bytes = vec![0u8; take];
            file.read_exact(&mut bytes)?;
            for (index, byte) in bytes.iter().copied().enumerate() {
                let absolute = batch_start + index as u64;
                if state_guard.partial_line.is_empty() && !state_guard.partial_oversized {
                    state_guard.partial_start = absolute;
                }
                if !state_guard.partial_oversized {
                    if state_guard.partial_line.len() < MAX_REVIEW_HISTORY_RECORD_BYTES {
                        state_guard.partial_line.push(byte);
                    } else {
                        state_guard.partial_line.clear();
                        state_guard.partial_oversized = true;
                    }
                }
                if byte == b'\n' {
                    if state_guard.partial_oversized {
                        state_guard.oversized_records =
                            state_guard.oversized_records.saturating_add(1);
                    } else {
                        let line = &state_guard.partial_line
                            [..state_guard.partial_line.len().saturating_sub(1)];
                        match serde_json::from_slice::<crate::telemetry::McpCallRecord>(line) {
                            Ok(record) => {
                                if record.tool == "search" {
                                    let id = mcp_search_id(&record);
                                    let location = SearchHistoryLocation {
                                        offset: state_guard.partial_start,
                                        length: absolute + 1 - state_guard.partial_start,
                                    };
                                    if let Some(previous) = state_guard.locations.get(&id) {
                                        if previous.offset != location.offset
                                            || previous.length != location.length
                                        {
                                            state_guard.ambiguous_ids.insert(id);
                                        }
                                    } else {
                                        state_guard
                                            .ordered_searches
                                            .push((location.offset, id.clone()));
                                        state_guard.locations.insert(id, location);
                                    }
                                }
                            }
                            Err(_) => {
                                state_guard.malformed_records =
                                    state_guard.malformed_records.saturating_add(1);
                                let malformed_offset = state_guard.partial_start;
                                state_guard
                                    .first_malformed_offset
                                    .get_or_insert(malformed_offset);
                            }
                        }
                    }
                    state_guard.partial_line.clear();
                    state_guard.partial_oversized = false;
                    state_guard.partial_start = absolute + 1;
                }
            }
            state_guard.scanned_offset = batch_start + bytes.len() as u64;
            state_guard.last_error = None;
            Ok(())
        })();
        if let Err(error) = result {
            let mut state_guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
            state_guard.last_error = Some(error.to_string());
        }
        std::thread::sleep(SEARCH_HISTORY_INDEX_POLL);
    }
}

fn read_search_history_location(
    history_path: &Path,
    expected_generation: &str,
    location: SearchHistoryLocation,
    expected_id: &str,
) -> anyhow::Result<crate::telemetry::McpCallRecord> {
    use std::io::{Read, Seek, SeekFrom};
    anyhow::ensure!(
        location.length > 0 && location.length <= MAX_REVIEW_HISTORY_RECORD_BYTES as u64,
        "indexed search record has an invalid length"
    );
    let mut file = std::fs::File::open(history_path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        search_history_generation(&metadata) == expected_generation
            && location
                .offset
                .checked_add(location.length)
                .is_some_and(|end| end <= metadata.len()),
        "search history changed; the search index is being rebuilt"
    );
    file.seek(SeekFrom::Start(location.offset))?;
    let mut bytes = vec![0; location.length as usize];
    file.read_exact(&mut bytes)?;
    anyhow::ensure!(
        bytes.last() == Some(&b'\n'),
        "indexed search record is incomplete"
    );
    let record: crate::telemetry::McpCallRecord =
        serde_json::from_slice(&bytes[..bytes.len() - 1])?;
    anyhow::ensure!(
        record.tool == "search" && mcp_search_id(&record) == expected_id,
        "indexed search record no longer matches its search_id"
    );
    Ok(record)
}

enum SearchHistoryLookup {
    Found(crate::telemetry::McpCallRecord),
    Pending(serde_json::Value),
    Incomplete {
        malformed_records: usize,
        oversized_records: usize,
        first_malformed_offset: Option<u64>,
    },
    Absent,
}
const MAX_DASHBOARD_ROW_BYTES: usize = 512 * 1024;

#[derive(Serialize)]
struct ReviewSourceDocument<'a> {
    doc_id: &'a str,
    source: &'a str,
    content: &'a str,
    concept: &'a str,
    group_id: &'a Option<String>,
    headings: &'a [String],
    links: &'a [String],
    timestamp: &'a Option<String>,
    doc_length: usize,
    author_agent: &'a Option<String>,
    filters: &'a BTreeMap<String, String>,
    key_phrases: &'a [crate::source::KeyPhrase],
    key_phrase_extraction_hash: &'a str,
}

/// Read one JSONL record without allowing a single telemetry response to
/// allocate an unbounded buffer. Oversized records are drained to the next
/// newline so later calls remain reviewable.
fn read_bounded_history_line<R: std::io::BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<Option<(bool, bool)>> {
    line.clear();
    let mut oversized = false;
    let mut consumed_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(consumed_any.then_some((oversized, false)));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let terminated = available[take - 1] == b'\n';
        if !oversized {
            if line.len().saturating_add(take) <= max_bytes {
                line.extend_from_slice(&available[..take]);
            } else {
                oversized = true;
                line.clear();
            }
        }
        reader.consume(take);
        consumed_any = true;
        if terminated {
            return Ok(Some((oversized, true)));
        }
    }
}

#[cfg(test)]
fn find_mcp_search_record(
    history_path: &Path,
    search_id: &str,
) -> anyhow::Result<crate::telemetry::McpCallRecord> {
    find_mcp_search_record_with_limit(history_path, search_id, MAX_REVIEW_HISTORY_RECORD_BYTES)
}

#[cfg(test)]
pub(super) fn find_mcp_search_record_with_limit(
    history_path: &Path,
    search_id: &str,
    max_record_bytes: usize,
) -> anyhow::Result<crate::telemetry::McpCallRecord> {
    let file = std::fs::File::open(history_path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut skipped_oversized = false;
    while let Some((oversized, terminated)) =
        read_bounded_history_line(&mut reader, &mut line, max_record_bytes)?
    {
        if oversized {
            skipped_oversized = true;
            continue;
        }
        if !terminated {
            break;
        }
        let Ok(record) = serde_json::from_slice::<crate::telemetry::McpCallRecord>(
            &line[..line.len().saturating_sub(1)],
        ) else {
            continue;
        };
        if record.tool == "search" && mcp_search_id(&record) == search_id {
            return Ok(record);
        }
    }
    if skipped_oversized {
        anyhow::bail!(
            "search_id was not found in MCP call history; oversized history records were skipped"
        );
    }
    anyhow::bail!("search_id was not found in MCP call history")
}

fn source_document_contains_term(document: &SourceDocument, term: &str) -> bool {
    let term = term.to_lowercase();
    let contains = |value: &str| value.to_lowercase().contains(&term);
    contains(&document.doc_id)
        || contains(&document.source)
        || contains(&document.content)
        || contains(&document.concept)
        || document.group_id.as_deref().is_some_and(contains)
        || document.headings.iter().any(|value| contains(value))
        || document.links.iter().any(|value| contains(value))
        || document.timestamp.as_deref().is_some_and(contains)
        || document.author_agent.as_deref().is_some_and(contains)
        || document
            .filters
            .iter()
            .any(|(key, value)| contains(key) || contains(value))
        || document
            .key_phrases
            .iter()
            .any(|phrase| contains(&phrase.text) || contains(&phrase.kind))
        || contains(&document.key_phrase_extraction_hash)
}

pub(super) fn mcp_search_id(record: &crate::telemetry::McpCallRecord) -> String {
    record.trace_id.clone().unwrap_or_else(|| {
        let encoded = serde_json::to_vec(record).unwrap_or_default();
        format!("mcp:legacy:{:x}", Sha256::digest(encoded))
    })
}

fn search_result_payload(record: &crate::telemetry::McpCallRecord) -> serde_json::Value {
    record
        .response
        .pointer("/result/content/0/text")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
pub(super) fn search_feedback_evaluation(
    reviews: &serde_json::Map<String, serde_json::Value>,
    records: &HashMap<String, crate::telemetry::McpCallRecord>,
) -> serde_json::Value {
    let mut overall_labels = BTreeMap::<String, usize>::new();
    let mut relevance_labels = BTreeMap::<String, usize>::new();
    let mut judged_searches = 0usize;
    let mut judged_documents = 0usize;
    let (mut ndcg5_sum, mut ndcg10_sum, mut ndcg5_n, mut ndcg10_n, mut mrr_sum) =
        (0.0, 0.0, 0usize, 0usize, 0.0);

    for (search_id, entry) in reviews {
        let Some(feedback) = entry.get("feedback") else {
            continue;
        };
        if let Some(label) = feedback.get("label").and_then(serde_json::Value::as_str) {
            *overall_labels.entry(label.to_string()).or_default() += 1;
        }
        let judgments = feedback
            .get("document_judgments")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut by_doc = HashMap::new();
        for judgment in &judgments {
            let (Some(doc_id), Some(relevance)) = (
                judgment.get("doc_id").and_then(serde_json::Value::as_str),
                judgment
                    .get("relevance")
                    .and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            *relevance_labels.entry(relevance.to_string()).or_default() += 1;
            by_doc.insert(doc_id.to_string(), relevance);
        }
        judged_documents += by_doc.len();

        let Some(record) = records.get(search_id) else {
            continue;
        };
        let result = search_result_payload(record);
        let Some(hits) = result.get("results").and_then(serde_json::Value::as_array) else {
            continue;
        };
        if hits.is_empty() || hits.len() != by_doc.len() {
            continue;
        }
        let ranked = hits
            .iter()
            .enumerate()
            .map(|(rank, hit)| {
                let doc_id = hit
                    .get("doc_id")
                    .or_else(|| hit.get("id"))
                    .and_then(serde_json::Value::as_str);
                doc_id
                    .and_then(|id| by_doc.get(id).copied())
                    .map(|relevance| {
                        let gain = match relevance {
                            "relevant" => 3.0,
                            "partially_relevant" => 1.0,
                            _ => 0.0,
                        };
                        (rank + 1, gain, relevance == "relevant")
                    })
            })
            .collect::<Option<Vec<_>>>();
        let Some(ranked) = ranked else { continue };
        let hit_ids = hits
            .iter()
            .filter_map(|hit| {
                hit.get("doc_id")
                    .or_else(|| hit.get("id"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<HashSet<_>>();
        if hit_ids.len() != hits.len()
            || hit_ids.len() != by_doc.len()
            || !hit_ids.iter().all(|id| by_doc.contains_key(*id))
        {
            continue;
        }
        judged_searches += 1;

        let dcg = |cutoff: usize| {
            ranked
                .iter()
                .take(cutoff)
                .enumerate()
                .map(|(index, (_, gain, _))| gain / ((index + 2) as f64).log2())
                .sum::<f64>()
        };
        let mut ideal_gains = ranked.iter().map(|(_, gain, _)| *gain).collect::<Vec<_>>();
        ideal_gains.sort_by(|a, b| b.total_cmp(a));
        let ideal = |cutoff: usize| {
            ideal_gains
                .iter()
                .take(cutoff)
                .enumerate()
                .map(|(index, gain)| gain / ((index + 2) as f64).log2())
                .sum::<f64>()
        };
        let idcg5 = ideal(5);
        let idcg10 = ideal(10);
        if idcg5 > 0.0 {
            ndcg5_sum += dcg(5) / idcg5;
            ndcg5_n += 1;
        }
        if idcg10 > 0.0 {
            ndcg10_sum += dcg(10) / idcg10;
            ndcg10_n += 1;
        }
        mrr_sum += ranked
            .iter()
            .find(|(_, _, relevant)| *relevant)
            .map(|(rank, _, _)| 1.0 / *rank as f64)
            .unwrap_or(0.0);
    }

    let metric = |sum: f64, n: usize| {
        serde_json::json!({
            "value": if n == 0 { serde_json::Value::Null } else { serde_json::json!(sum / n as f64) },
            "n": n,
        })
    };
    serde_json::json!({
        "reviewed_searches": reviews.values().filter(|entry| entry.get("feedback").is_some()).count(),
        "complete_judged_searches": judged_searches,
        "judged_documents": judged_documents,
        "overall_labels": overall_labels,
        "relevance_labels": relevance_labels,
        "ndcg_at_5": metric(ndcg5_sum, ndcg5_n),
        "ndcg_at_10": metric(ndcg10_sum, ndcg10_n),
        "mrr": metric(mrr_sum, judged_searches),
        "sample_status": if judged_searches < 30 { "small" } else { "usable" },
    })
}

fn search_history_cursor(
    path: &Path,
    generation: &str,
    offset: u64,
    snapshot_end: u64,
) -> anyhow::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        search_history_generation(&metadata) == generation
            && offset <= snapshot_end
            && snapshot_end <= metadata.len(),
        "search history changed; restart review from the first page"
    );
    if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        anyhow::ensure!(
            previous[0] == b'\n',
            "search history changed; cursor is not on a record boundary"
        );
    }
    Ok(format!("v2:{generation}:{offset}:{snapshot_end}"))
}

fn parse_search_history_cursor(path: &Path, cursor: &str) -> anyhow::Result<(u64, u64)> {
    let (generation, offset, snapshot_end) = parse_search_history_cursor_fields(cursor)?;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        search_history_generation(&metadata) == generation
            && offset <= snapshot_end
            && snapshot_end <= metadata.len(),
        "search history changed; restart review from the first page"
    );
    if offset > 0 {
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        anyhow::ensure!(
            previous[0] == b'\n',
            "search history changed; cursor is not on a record boundary"
        );
    }
    Ok((offset, snapshot_end))
}

pub(super) fn parse_search_history_cursor_fields(cursor: &str) -> anyhow::Result<(&str, u64, u64)> {
    let mut parts = cursor.rsplitn(3, ':');
    let snapshot_end = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid search history cursor"))?;
    let offset = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid search history cursor"))?;
    let generation = parts
        .next()
        .and_then(|value| value.strip_prefix("v2:"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("unsupported search history cursor; restart review from the first page")
        })?;
    Ok((generation, offset, snapshot_end))
}
