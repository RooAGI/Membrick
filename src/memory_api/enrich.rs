//! Source document enrichment from reviewed searches.

use super::*;

impl MemoryService {
    /// Merge grounded terminology into one existing source document. The
    /// caller must provide the full-source hash it reviewed; stale proposals
    /// are rejected, while identical retries are no-ops. The source document
    /// remains the sole searchable representation.
    pub fn enrich_source_document(
        &mut self,
        enrichment: crate::source::SourceDocumentEnrichment,
    ) -> anyhow::Result<bool> {
        Ok(self.enrich_source_documents(vec![enrichment])? > 0)
    }

    /// Validate and stage a set of additive source-document enrichments as
    /// one service mutation. Every expected hash is checked before any source
    /// is changed, so a stale or invalid member cannot leave a partial batch.
    /// The resident writer publishes the resulting source records through its
    /// normal staged index path.
    pub fn enrich_source_documents(
        &mut self,
        enrichments: Vec<crate::source::SourceDocumentEnrichment>,
    ) -> anyhow::Result<usize> {
        Ok(self
            .enrich_source_documents_for_operation(enrichments, None)?
            .len())
    }

    pub(super) fn enrich_source_documents_for_operation(
        &mut self,
        enrichments: Vec<crate::source::SourceDocumentEnrichment>,
        operation_id: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let replacements = self.prepare_source_document_enrichments(enrichments, operation_id)?;
        self.apply_prepared_source_document_enrichments(replacements)
    }

    pub(super) fn prepare_source_document_enrichments(
        &self,
        enrichments: Vec<crate::source::SourceDocumentEnrichment>,
        operation_id: Option<&str>,
    ) -> anyhow::Result<Vec<(crate::SourceDocument, bool)>> {
        use crate::source::{KeyPhrase, KeyPhraseOrigin};
        use std::collections::HashSet;

        let mut seen_ids = HashSet::new();
        let mut replacements = Vec::new();
        for enrichment in enrichments {
            let original_enrichment = enrichment.clone();
            anyhow::ensure!(!enrichment.doc_id.trim().is_empty(), "doc_id is required");
            anyhow::ensure!(
                seen_ids.insert(enrichment.doc_id.clone()),
                "enrichment batch contains duplicate document id {:?}",
                enrichment.doc_id
            );
            let captured = self.store.source_document_by_id(&enrichment.doc_id);
            let workspace = self
                .workspace_store
                .as_deref()
                .and_then(|store| store.store.source_document_by_id(&enrichment.doc_id));
            anyhow::ensure!(
                !(captured.is_some() && workspace.is_some()),
                "enrichment target document ID is ambiguous across MemoryService stores"
            );
            let (current, workspace_owned) = if let Some(document) = captured {
                (document.clone(), false)
            } else if let Some(document) = workspace {
                (document.clone(), true)
            } else {
                anyhow::bail!("enrichment target document was not found");
            };

            let is_session_context = current
                .filters
                .get("document_type")
                .is_some_and(|kind| kind == "session-context");
            anyhow::ensure!(
                (enrichment.session_conclusion.is_none() && enrichment.headings.is_empty())
                    || is_session_context,
                "session conclusions and generated headings require a session-context document"
            );

            let mut additions = Vec::<KeyPhrase>::new();
            for mut phrase in enrichment.key_phrases {
                phrase.text = phrase.text.trim().to_string();
                phrase.kind = phrase.kind.trim().to_ascii_lowercase();
                anyhow::ensure!(!phrase.text.is_empty(), "key phrase text must not be blank");
                anyhow::ensure!(
                    matches!(
                        phrase.kind.as_str(),
                        "event" | "place" | "org" | "work" | "thing"
                    ),
                    "unsupported key phrase kind {:?}",
                    phrase.kind
                );
                phrase.origin = Some(KeyPhraseOrigin::ProviderEnrichment);
                let duplicate = current
                    .key_phrases
                    .iter()
                    .chain(&additions)
                    .any(|existing| {
                        existing.text.eq_ignore_ascii_case(&phrase.text)
                            && existing.kind.eq_ignore_ascii_case(&phrase.kind)
                    });
                if !duplicate {
                    additions.push(phrase);
                }
            }

            let mut headings = Vec::new();
            for heading in enrichment.headings {
                let heading = heading.trim().to_string();
                anyhow::ensure!(!heading.is_empty(), "heading must not be blank");
                if !current
                    .headings
                    .iter()
                    .chain(&headings)
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&heading))
                {
                    headings.push(heading);
                }
            }

            let mut links = Vec::new();
            for link in enrichment.links {
                let link = link.trim().to_string();
                anyhow::ensure!(!link.is_empty(), "link target must not be blank");
                anyhow::ensure!(link != current.doc_id, "a document cannot link to itself");
                let exists = self.store.source_document_by_id(&link).is_some()
                    || self
                        .workspace_store
                        .as_deref()
                        .and_then(|store| store.store.source_document_by_id(&link))
                        .is_some();
                anyhow::ensure!(exists, "link target document was not found");
                if !current.links.contains(&link) && !links.contains(&link) {
                    links.push(link);
                }
            }

            let supersedes_document_id = enrichment
                .supersedes_document_id
                .map(|value| value.trim().to_string());
            let supersedes_changed = if let Some(target_id) = supersedes_document_id.as_ref() {
                anyhow::ensure!(
                    !target_id.is_empty(),
                    "superseded document ID must not be blank"
                );
                anyhow::ensure!(
                    target_id != &current.doc_id,
                    "a document cannot supersede itself"
                );
                let target_captured = self.store.source_document_by_id(target_id);
                let target_workspace = self
                    .workspace_store
                    .as_deref()
                    .and_then(|store| store.store.source_document_by_id(target_id));
                anyhow::ensure!(
                    !(target_captured.is_some() && target_workspace.is_some()),
                    "superseded document ID is ambiguous across MemoryService stores"
                );
                let target = target_captured
                    .or(target_workspace)
                    .ok_or_else(|| anyhow::anyhow!("superseded document was not found"))?;
                let target_workspace_owned = target_workspace.is_some();
                anyhow::ensure!(
                    workspace_owned == target_workspace_owned,
                    "superseded document must be in the same backing store"
                );
                anyhow::ensure!(
                    crate::semantic_relations::semantic_scope(&current)
                        == crate::semantic_relations::semantic_scope(target),
                    "superseded document is outside the target's semantic scope"
                );
                if let Some(existing) = current.filters.get("supersedes_id") {
                    anyhow::ensure!(
                        existing == target_id,
                        "source document already supersedes a different document"
                    );
                    false
                } else {
                    true
                }
            } else {
                false
            };

            let conclusion = enrichment
                .session_conclusion
                .map(|value| value.trim().to_string());
            if let Some(value) = conclusion.as_ref() {
                anyhow::ensure!(!value.is_empty(), "session conclusion must not be blank");
            }
            let conclusion_changed = conclusion.as_ref().is_some_and(|value| {
                current
                    .filters
                    .get("membrick_session_conclusion")
                    .is_none_or(|existing| existing != value)
            });

            if additions.is_empty()
                && headings.is_empty()
                && links.is_empty()
                && !supersedes_changed
                && !conclusion_changed
            {
                // A retry, or a provider phrase already produced by the
                // extractor, needs no write and cannot overwrite newer data.
                continue;
            }

            let actual_hash = crate::source::source_document_hash(&current)?;
            anyhow::ensure!(
                actual_hash == enrichment.expected_source_hash,
                "source document changed after enrichment work was prepared"
            );
            let mut replacement = current;
            replacement.key_phrases.extend(additions);
            replacement.headings.extend(headings);
            let contributed_link_ids = links.iter().cloned().collect::<HashSet<_>>();
            replacement.links.extend(links);
            if supersedes_changed {
                replacement.filters.insert(
                    "supersedes_id".to_string(),
                    supersedes_document_id.expect("supersedes_changed requires a target"),
                );
            }
            if let Some(conclusion) = conclusion.filter(|_| conclusion_changed) {
                set_session_conclusion(&mut replacement, &conclusion);
            }
            if let Some(operation_id) = operation_id {
                replacement.filters.insert(
                    "membrick_last_enrichment_search_id".to_string(),
                    operation_id.to_string(),
                );
                let mut contribution = original_enrichment;
                contribution.key_phrases = contribution
                    .key_phrases
                    .into_iter()
                    .filter_map(|mut phrase| {
                        let present = replacement.key_phrases.iter().any(|existing| {
                            existing.origin
                                == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
                                && existing.text.eq_ignore_ascii_case(phrase.text.trim())
                                && existing.kind.eq_ignore_ascii_case(phrase.kind.trim())
                        });
                        if present {
                            phrase.origin =
                                Some(crate::source::KeyPhraseOrigin::ProviderEnrichment);
                            Some(phrase)
                        } else {
                            None
                        }
                    })
                    .collect();
                contribution.headings.retain(|heading| {
                    replacement
                        .headings
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(heading.trim()))
                });
                contribution
                    .links
                    .retain(|link| contributed_link_ids.contains(&link.trim().to_string()));
                contribution.session_conclusion = conclusion_changed
                    .then(|| {
                        replacement
                            .filters
                            .get("membrick_session_conclusion")
                            .cloned()
                    })
                    .flatten();
                if let Some(target_id) = replacement.filters.get("supersedes_id") {
                    contribution.supersedes_document_id = Some(target_id.clone());
                }
                for evidence in &mut contribution.evidence {
                    let source = self
                        .store
                        .source_document_by_id(&evidence.doc_id)
                        .or_else(|| {
                            self.workspace_store.as_deref().and_then(|store| {
                                store.store.source_document_by_id(&evidence.doc_id)
                            })
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("enrichment evidence document was not found")
                        })?;
                    evidence.expected_source_hash = source_content_fingerprint(source);
                }
                if !contribution.key_phrases.is_empty()
                    || !contribution.headings.is_empty()
                    || !contribution.links.is_empty()
                    || contribution.supersedes_document_id.is_some()
                    || contribution.session_conclusion.is_some()
                {
                    replacement.filters.insert(
                        enrichment_contribution_filter(operation_id),
                        serde_json::to_string(&contribution)?,
                    );
                }
            }
            replacement
                .headings
                .sort_by_key(|heading| heading.to_ascii_lowercase());
            replacement
                .headings
                .dedup_by(|left, right| left.eq_ignore_ascii_case(right));
            replacement.links.sort();
            replacement.links.dedup();
            replacement.doc_length = replacement.content.len();
            replacement.key_phrases.sort_by(|left, right| {
                let left_origin = serde_json::to_string(&left.origin).unwrap_or_default();
                let right_origin = serde_json::to_string(&right.origin).unwrap_or_default();
                left_origin
                    .cmp(&right_origin)
                    .then_with(|| {
                        left.kind
                            .to_ascii_lowercase()
                            .cmp(&right.kind.to_ascii_lowercase())
                    })
                    .then_with(|| {
                        left.text
                            .to_ascii_lowercase()
                            .cmp(&right.text.to_ascii_lowercase())
                    })
            });
            replacements.push((replacement, workspace_owned));
        }

        Ok(replacements)
    }

    pub(super) fn apply_prepared_source_document_enrichments(
        &mut self,
        replacements: Vec<(crate::SourceDocument, bool)>,
    ) -> anyhow::Result<Vec<String>> {
        if replacements.is_empty() {
            return Ok(Vec::new());
        }
        let changed_doc_ids = replacements
            .iter()
            .map(|(document, _)| document.doc_id.clone())
            .collect::<Vec<_>>();
        let (captured, workspace): (Vec<_>, Vec<_>) = replacements
            .into_iter()
            .partition(|(_, workspace_owned)| !workspace_owned);
        if !captured.is_empty() {
            self.stage_upsert_batch(captured.into_iter().map(|(document, _)| document).collect())?;
        }
        if !workspace.is_empty() {
            let store = self
                .workspace_store
                .as_deref_mut()
                .ok_or_else(|| anyhow::anyhow!("workspace enrichment owner is unavailable"))?;
            store.stage_upsert_batch(
                workspace
                    .into_iter()
                    .map(|(document, _)| document)
                    .collect(),
            )?;
        }
        Ok(changed_doc_ids)
    }

    /// Preserve provider-approved additions on generated session-context
    /// documents when a hook publishes the next capture batch.
    pub(crate) fn preserve_document_enrichment(
        &self,
        target: MemoryStore,
        documents: &mut [SourceDocument],
    ) {
        let existing_store = self.memory_store(target);
        let incoming_fingerprints = documents
            .iter()
            .map(|document| {
                (
                    document.doc_id.clone(),
                    source_content_fingerprint(document),
                )
            })
            .collect::<HashMap<_, _>>();
        for incoming in documents {
            let Some(existing) = existing_store
                .store
                .source_document_by_id(&incoming.doc_id)
                .cloned()
            else {
                continue;
            };

            let contributions = existing
                .filters
                .iter()
                .filter(|(key, _)| key.starts_with(ENRICHMENT_CONTRIBUTION_PREFIX))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>();
            if incoming.filters.get("document_type").map(String::as_str) == Some("session-context")
            {
                // Hook-generated observation links are part of the complete
                // session record, not an optional provider enrichment. Keep
                // them across captures even when the feedback contribution
                // only added a conclusion or headings. Links owned by a
                // feedback contribution are merged below only if its cited
                // evidence is still current.
                let contributed_links = contributions
                    .iter()
                    .filter_map(|(_, encoded)| {
                        serde_json::from_str::<crate::source::SourceDocumentEnrichment>(encoded)
                            .ok()
                    })
                    .flat_map(|contribution| contribution.links)
                    .collect::<HashSet<_>>();
                for link in &existing.links {
                    if contributed_links.contains(link) {
                        continue;
                    }
                    let target_exists = incoming_fingerprints.contains_key(link)
                        || self.store.source_document_by_id(link).is_some()
                        || self
                            .workspace_store
                            .as_deref()
                            .and_then(|store| store.store.source_document_by_id(link))
                            .is_some();
                    if target_exists && !incoming.links.contains(link) {
                        incoming.links.push(link.clone());
                    }
                }
            }
            if contributions.is_empty()
                && incoming.filters.get("document_type").map(String::as_str)
                    == Some("session-context")
            {
                // Preserve context enrichment created before per-operation
                // contribution records were introduced.
                for (key, value) in &existing.filters {
                    if key.starts_with("membrick_") {
                        incoming.filters.insert(key.clone(), value.clone());
                    }
                }
                for heading in &existing.headings {
                    if !incoming
                        .headings
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(heading))
                    {
                        incoming.headings.push(heading.clone());
                    }
                }
                for link in &existing.links {
                    if !incoming.links.contains(link) {
                        incoming.links.push(link.clone());
                    }
                }
                for phrase in existing.key_phrases.iter().filter(|phrase| {
                    phrase.origin == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
                }) {
                    if !incoming.key_phrases.iter().any(|current| {
                        current.text.eq_ignore_ascii_case(&phrase.text)
                            && current.kind.eq_ignore_ascii_case(&phrase.kind)
                    }) {
                        incoming.key_phrases.push(phrase.clone());
                    }
                }
                if let Some(conclusion) = existing.filters.get("membrick_session_conclusion") {
                    set_session_conclusion(incoming, conclusion);
                }
                incoming.doc_length = incoming.content.len();
                continue;
            }

            let mut valid_operation_ids = HashSet::new();
            for (key, encoded) in contributions {
                let Ok(contribution) =
                    serde_json::from_str::<crate::source::SourceDocumentEnrichment>(&encoded)
                else {
                    incoming.filters.remove(&key);
                    continue;
                };
                let dependencies_match = contribution.evidence.iter().all(|evidence| {
                    let actual = incoming_fingerprints
                        .get(&evidence.doc_id)
                        .cloned()
                        .or_else(|| {
                            self.store
                                .source_document_by_id(&evidence.doc_id)
                                .map(source_content_fingerprint)
                        })
                        .or_else(|| {
                            self.workspace_store
                                .as_deref()
                                .and_then(|store| {
                                    store.store.source_document_by_id(&evidence.doc_id)
                                })
                                .map(source_content_fingerprint)
                        });
                    actual.as_deref() == Some(evidence.expected_source_hash.as_str())
                });

                if dependencies_match {
                    for phrase in contribution.key_phrases {
                        if !incoming.key_phrases.iter().any(|current| {
                            current.text.eq_ignore_ascii_case(&phrase.text)
                                && current.kind.eq_ignore_ascii_case(&phrase.kind)
                        }) {
                            incoming.key_phrases.push(phrase);
                        }
                    }
                    for heading in contribution.headings {
                        if !incoming
                            .headings
                            .iter()
                            .any(|current| current.eq_ignore_ascii_case(&heading))
                        {
                            incoming.headings.push(heading);
                        }
                    }
                    for link in contribution.links {
                        if !incoming.links.contains(&link) {
                            incoming.links.push(link);
                        }
                    }
                    if let Some(target_id) = contribution.supersedes_document_id {
                        incoming
                            .filters
                            .insert("supersedes_id".to_string(), target_id);
                    }
                    if let Some(conclusion) = contribution.session_conclusion {
                        set_session_conclusion(incoming, &conclusion);
                    }
                    valid_operation_ids.insert(key);
                } else {
                    for phrase in contribution.key_phrases {
                        incoming.key_phrases.retain(|current| {
                            !(current.origin
                                == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
                                && current.text.eq_ignore_ascii_case(&phrase.text)
                                && current.kind.eq_ignore_ascii_case(&phrase.kind))
                        });
                    }
                    for heading in contribution.headings {
                        incoming
                            .headings
                            .retain(|current| !current.eq_ignore_ascii_case(&heading));
                    }
                    for link in contribution.links {
                        incoming.links.retain(|current| current != &link);
                    }
                    if contribution
                        .supersedes_document_id
                        .as_ref()
                        .is_some_and(|target| incoming.filters.get("supersedes_id") == Some(target))
                    {
                        incoming.filters.remove("supersedes_id");
                    }
                    if contribution
                        .session_conclusion
                        .as_ref()
                        .is_some_and(|conclusion| {
                            incoming
                                .filters
                                .get("membrick_session_conclusion")
                                .is_some_and(|current| current == conclusion)
                        })
                    {
                        remove_session_conclusion(incoming);
                    }
                    incoming.filters.remove(&key);
                }
            }
            for (key, value) in &existing.filters {
                if key.starts_with(ENRICHMENT_CONTRIBUTION_PREFIX)
                    && valid_operation_ids.contains(key)
                {
                    incoming.filters.insert(key.clone(), value.clone());
                }
            }
            if let Some(last_operation) = existing.filters.get("membrick_last_enrichment_search_id")
            {
                let key = enrichment_contribution_filter(last_operation);
                if valid_operation_ids.contains(&key) {
                    incoming.filters.insert(
                        "membrick_last_enrichment_search_id".to_string(),
                        last_operation.clone(),
                    );
                }
            }
            incoming.doc_length = incoming.content.len();
        }
    }

    /// Enrich one selected source document through MemoryService. The target
    /// and each evidence document are identified by exact source revisions.
    pub(crate) fn enrich_doc_mcp(
        &mut self,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let args = arguments
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("enrich_doc arguments must be an object"))?;
        const ALLOWED: &[&str] = &[
            "doc_id",
            "expected_source_hash",
            "key_phrases",
            "session_conclusion",
            "headings",
            "links",
            "supersedes_document_id",
            "evidence",
        ];
        if let Some(unknown) = args.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
            anyhow::bail!("unknown enrich_doc argument: {unknown}");
        }
        let enrichment: crate::source::SourceDocumentEnrichment =
            serde_json::from_value(arguments.clone())?;
        anyhow::ensure!(
            !enrichment.evidence.is_empty(),
            "enrich_doc requires evidence"
        );

        let source_doc = |service: &MemoryService, doc_id: &str| {
            let captured = service.store.source_document_by_id(doc_id);
            let workspace = service
                .workspace_store
                .as_deref()
                .and_then(|store| store.store.source_document_by_id(doc_id));
            anyhow::ensure!(
                !(captured.is_some() && workspace.is_some()),
                "evidence document ID is ambiguous across MemoryService stores"
            );
            captured
                .or(workspace)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("evidence document was not found: {doc_id}"))
        };
        let target_is_workspace = self
            .store
            .source_document_by_id(&enrichment.doc_id)
            .is_none()
            && self
                .workspace_store
                .as_deref()
                .and_then(|store| store.store.source_document_by_id(&enrichment.doc_id))
                .is_some();
        let target = source_doc(self, &enrichment.doc_id)?;
        anyhow::ensure!(
            crate::source::source_document_hash(&target)? == enrichment.expected_source_hash
                || enrichment_is_present(&target, &enrichment),
            "target source hash does not match the current document"
        );
        let mut evidence_ids = HashSet::new();
        for evidence in &enrichment.evidence {
            anyhow::ensure!(
                evidence_ids.insert(evidence.doc_id.as_str()),
                "duplicate evidence document ID"
            );
            let document = source_doc(self, &evidence.doc_id)?;
            anyhow::ensure!(
                crate::source::source_document_hash(&document)? == evidence.expected_source_hash,
                "evidence source hash does not match the current document"
            );
        }
        if let Some(replaced) = enrichment.supersedes_document_id.as_deref() {
            anyhow::ensure!(
                evidence_ids.contains(replaced),
                "superseded document must be cited as evidence"
            );
        }

        let operation_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&enrichment)?));
        let operation_id = format!("enrich_doc:{operation_hash}");
        let target_id = enrichment.doc_id.clone();
        let supersedes = enrichment.supersedes_document_id.is_some();
        let changed =
            self.enrich_source_documents_for_operation(vec![enrichment], Some(&operation_id))?;
        if changed
            .iter()
            .any(|id| self.store.source_document_by_id(id).is_some())
        {
            self.flush()?;
        }
        if changed.iter().any(|id| {
            self.workspace_store
                .as_deref()
                .is_some_and(|store| store.store.source_document_by_id(id).is_some())
        }) {
            self.workspace_store
                .as_deref_mut()
                .ok_or_else(|| anyhow::anyhow!("workspace enrichment owner is unavailable"))?
                .flush()?;
        }
        let adjudication = if supersedes {
            let owner = if target_is_workspace {
                &self
                    .workspace_store
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("workspace enrichment owner is unavailable"))?
                    .store
            } else {
                &self.store
            };
            Some(self.build_adjudication_receipt_from_store(
                owner,
                operation_id.clone(),
                &[target_id.clone()],
            ))
        } else {
            None
        };
        Ok(serde_json::json!({
            "doc_id": target_id,
            "status": if changed.is_empty() { "unchanged" } else { "enriched" },
            "changed_documents": changed.len(),
            "changed_document_ids": changed,
            "publication_status": "published",
            "adjudication": adjudication,
        }))
    }
}

fn set_session_conclusion(document: &mut SourceDocument, conclusion: &str) {
    const CONCLUSION_FILTER: &str = "membrick_session_conclusion";
    const CONCLUSION_HEADING: &str = "Session conclusion:";
    if let Some(previous) = document.filters.get(CONCLUSION_FILTER) {
        let previous_block = format!("\n\n{CONCLUSION_HEADING}\n{previous}");
        if let Some(base) = document.content.strip_suffix(&previous_block) {
            document.content = base.into();
        }
    }
    let base = document.content.trim_end();
    document.content = format!("{base}\n\n{CONCLUSION_HEADING}\n{conclusion}").into();
    document
        .filters
        .insert(CONCLUSION_FILTER.to_string(), conclusion.to_string());
    document.doc_length = document.content.len();
}

fn remove_session_conclusion(document: &mut SourceDocument) {
    const CONCLUSION_FILTER: &str = "membrick_session_conclusion";
    const CONCLUSION_HEADING: &str = "Session conclusion:";
    if let Some(previous) = document.filters.remove(CONCLUSION_FILTER) {
        let previous_block = format!("\n\n{CONCLUSION_HEADING}\n{previous}");
        if let Some(base) = document.content.strip_suffix(&previous_block) {
            document.content = base.into();
            document.doc_length = document.content.len();
        }
    }
}

pub(super) const ENRICHMENT_CONTRIBUTION_PREFIX: &str = "membrick_enrichment_contribution:";

fn enrichment_contribution_filter(operation_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(operation_id.as_bytes());
    let suffix = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{ENRICHMENT_CONTRIBUTION_PREFIX}{suffix}")
}

fn source_content_fingerprint(document: &SourceDocument) -> String {
    crate::pipeline::key_phrase_content_hash(&document.content)
}

fn enrichment_is_present(
    document: &SourceDocument,
    enrichment: &crate::source::SourceDocumentEnrichment,
) -> bool {
    let has_contribution = !enrichment.key_phrases.is_empty()
        || !enrichment.headings.is_empty()
        || !enrichment.links.is_empty()
        || enrichment.supersedes_document_id.is_some()
        || enrichment.session_conclusion.is_some();
    has_contribution
        && enrichment.key_phrases.iter().all(|phrase| {
            document.key_phrases.iter().any(|existing| {
                existing.origin == Some(crate::source::KeyPhraseOrigin::ProviderEnrichment)
                    && existing.text.eq_ignore_ascii_case(phrase.text.trim())
                    && existing.kind.eq_ignore_ascii_case(phrase.kind.trim())
            })
        })
        && enrichment.headings.iter().all(|heading| {
            document
                .headings
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(heading.trim()))
        })
        && enrichment
            .links
            .iter()
            .all(|link| document.links.contains(&link.trim().to_string()))
        && enrichment
            .supersedes_document_id
            .as_ref()
            .is_none_or(|target| document.filters.get("supersedes_id") == Some(target))
        && enrichment
            .session_conclusion
            .as_ref()
            .is_none_or(|conclusion| {
                document
                    .filters
                    .get("membrick_session_conclusion")
                    .is_some_and(|existing| existing == conclusion.trim())
            })
}
