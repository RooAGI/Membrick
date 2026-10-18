//! Background and query-time key-phrase extraction for stored documents.

use super::search::RELATIONS_EXTRACT_TIMEOUT_SECS;
use super::*;

impl MemoryService {
    /// Extract key phrases for the given documents. Associated function
    /// (not a method) so the server can run it with no service lock held:
    /// it is slow (subprocess with its own timeout) and holding the lock
    /// would stall concurrent queries behind the extractor. `None` means
    /// the run timed out or the extractor failed (transient: retry later);
    /// `Some` means the extractor ran to completion.
    pub(crate) fn extract_key_phrases_for_docs(
        docs: &[SourceDocument],
        script: Option<&std::path::Path>,
    ) -> Option<Vec<crate::segments::relations::RawKeyPhrase>> {
        let refs: Vec<&SourceDocument> = docs.iter().collect();
        let turns = relation_turns_from_docs(&refs);
        run_key_phrase_extraction_bounded(&turns, script, KEY_PHRASE_BACKFILL_TIMEOUT_SECS)
    }

    /// Queue freshly written documents for background key-phrase
    /// enrichment. No-op when `key_phrase_enrichment` is off. The write
    /// itself already finished; this only schedules the async work.
    /// Also a no-op when `python_free()` — the extractor is spaCy-only.
    pub(super) fn queue_key_phrase_enrichment(&mut self, docs: &[SourceDocument]) {
        if !self.store.options().key_phrase_enrichment
            || self.store.options().python_free()
            || docs.is_empty()
        {
            return;
        }
        let refs: Vec<&SourceDocument> = docs.iter().collect();
        let hashes: HashMap<&str, String> = refs
            .iter()
            .map(|doc| {
                (
                    doc.doc_id.as_str(),
                    crate::pipeline::key_phrase_content_hash(&doc.content),
                )
            })
            .collect();
        // relation_turns_from_docs sorts by (session_id, turn_idx); join the
        // staleness hashes back by doc_id.
        let turns = relation_turns_from_docs(&refs);
        {
            let mut state = self.enrichment.lock().expect("enrichment lock poisoned");
            for turn in turns {
                let content_hash = hashes
                    .get(turn.doc_id.as_str())
                    .cloned()
                    .unwrap_or_default();
                // Latest write wins: drop any older queued turn for this doc.
                if let Some(pos) = state
                    .pending
                    .iter()
                    .position(|p| p.turn.doc_id == turn.doc_id)
                {
                    state.pending.remove(pos);
                }
                state.pending.push_back(PendingTurn { turn, content_hash });
                while state.pending.len() > KEY_PHRASE_PENDING_MAX {
                    state.pending.pop_front();
                }
            }
        }
        self.maybe_spawn_enrichment_worker();
    }

    /// Spawn the enrichment worker if none is running and work is pending.
    /// The worker drains the queue completely (batch by batch), running the
    /// extractor off-thread and posting results to the inbox; a later write
    /// landing mid-drain is picked up by the same worker.
    fn maybe_spawn_enrichment_worker(&self) {
        let script = self.store.options().extractor_script.clone();
        let state = std::sync::Arc::clone(&self.enrichment);
        {
            let mut guard = state.lock().expect("enrichment lock poisoned");
            if guard.worker_running || guard.pending.is_empty() {
                return;
            }
            guard.worker_running = true;
        }
        std::thread::spawn(move || {
            // Linger so rapid writes coalesce into one extractor run
            // instead of one model load per write.
            std::thread::sleep(std::time::Duration::from_millis(KEY_PHRASE_LINGER_MS));
            // Drain the queue completely: a single spawn handles every batch,
            // so no later write is needed to pick up stragglers. `worker_running`
            // stays true until the queue is empty, which coalesces any write
            // that lands mid-drain into this same worker.
            loop {
                let batch: Vec<PendingTurn> = {
                    let mut guard = state.lock().expect("enrichment lock poisoned");
                    if guard.pending.is_empty() {
                        guard.worker_running = false;
                        return;
                    }
                    let take = guard.pending.len().min(KEY_PHRASE_BATCH_SIZE);
                    guard.pending.drain(..take).collect()
                };
                let raw = {
                    let turns: Vec<_> = batch.iter().map(|p| p.turn.clone()).collect();
                    run_key_phrase_extraction_bounded(
                        &turns,
                        script.as_deref(),
                        RELATIONS_EXTRACT_TIMEOUT_SECS,
                    )
                };
                let mut guard = state.lock().expect("enrichment lock poisoned");
                let Some(phrases) = raw else {
                    // Transient failure (timeout): re-queue the batch so a
                    // later worker — or a query-time backfill — retries it.
                    // The documents stay un-stamped; stamping them now would
                    // mark a failed run as "done" permanently.
                    for pending in batch.into_iter().rev() {
                        guard.pending.push_front(pending);
                    }
                    guard.worker_running = false;
                    return;
                };
                for pending in &batch {
                    let doc_phrases: Vec<crate::source::KeyPhrase> = phrases
                        .iter()
                        .filter(|p| p.doc_id == pending.turn.doc_id)
                        .map(|p| crate::source::KeyPhrase {
                            text: p.text.clone(),
                            kind: p.kind.clone(),
                            origin: Some(crate::source::KeyPhraseOrigin::Extractor),
                        })
                        .collect();
                    guard.inbox.push(EnrichedDoc {
                        doc_id: pending.turn.doc_id.clone(),
                        content_hash: pending.content_hash.clone(),
                        phrases: doc_phrases,
                    });
                }
            }
        });
    }

    /// Apply finished enrichment batches to the store. Documents whose
    /// content changed while their batch was in flight are skipped as
    /// stale (their replacement re-queued itself via `add`/`update`).
    pub(super) fn drain_enrichment_inbox(&mut self) {
        let inbox: Vec<EnrichedDoc> = self
            .enrichment
            .lock()
            .expect("enrichment lock poisoned")
            .inbox
            .drain(..)
            .collect();
        if inbox.is_empty() {
            return;
        }
        for item in inbox {
            self.store
                .set_key_phrases(&item.doc_id, &item.content_hash, item.phrases);
        }
    }

    /// Drop queued and finished enrichment work for a deleted document.
    pub(super) fn drop_enrichment_for_doc(&self, doc_id: &str) {
        let mut state = self.enrichment.lock().expect("enrichment lock poisoned");
        state.pending.retain(|p| p.turn.doc_id != doc_id);
        state.inbox.retain(|e| e.doc_id != doc_id);
    }

    /// Whether any document still needs key-phrase extraction. Cheap scan
    /// with early exit: read-only callers (the server search handler) use it
    /// to decide whether a write-lock backfill is worthwhile, keeping the
    /// steady state at zero extra cost. False when `python_free()` — the
    /// extractor is spaCy-only.
    pub(crate) fn key_phrase_backfill_needed(&self) -> bool {
        if !self.store.options().key_phrase_enrichment || self.store.options().python_free() {
            return false;
        }
        if !self
            .enrichment
            .lock()
            .expect("enrichment lock poisoned")
            .pending
            .is_empty()
        {
            return true;
        }
        self.store
            .source_documents()
            .iter()
            .any(|doc| Self::doc_needs_key_phrases(doc))
    }

    /// True when `doc` has no key phrases and extraction has not completed
    /// for its current content. A document whose extraction finished with a
    /// legitimately empty phrase list carries a stamp matching its content
    /// hash, so it is not mistaken for pending work.
    fn doc_needs_key_phrases(doc: &SourceDocument) -> bool {
        if doc.content.trim().is_empty() || !doc.key_phrases.is_empty() {
            return false;
        }
        doc.key_phrase_extraction_hash != crate::pipeline::key_phrase_content_hash(&doc.content)
    }

    /// Read-only snapshot for a query-time backfill: the documents still
    /// needing key-phrase extraction (deterministic order, at most one
    /// extractor batch) plus the extractor script override. The caller runs
    /// the subprocess via [`extract_key_phrases_for_docs`] without holding
    /// any service lock, then applies the result with
    /// [`apply_key_phrase_backfill`]. Returns no documents when enrichment
    /// is disabled or when `python_free()` — the extractor is spaCy-only.
    pub(crate) fn key_phrase_backfill_snapshot(
        &self,
    ) -> (Vec<SourceDocument>, Option<std::path::PathBuf>) {
        if !self.store.options().key_phrase_enrichment || self.store.options().python_free() {
            return (Vec::new(), None);
        }
        // Deterministic order so repeated calls converge instead of
        // starving documents.
        let mut missing: Vec<&SourceDocument> = self
            .store
            .source_documents()
            .into_iter()
            .filter(|doc| Self::doc_needs_key_phrases(doc))
            .collect();
        missing.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        missing.truncate(KEY_PHRASE_BATCH_SIZE);
        let docs: Vec<SourceDocument> = missing.into_iter().cloned().collect();
        let script = self.store.options().extractor_script.clone();
        (docs, script)
    }

    /// Apply extractor output from a query-time backfill: first apply
    /// anything the background worker already finished, then validate
    /// content hashes and stamp each document (a legitimately empty phrase
    /// list still gets its stamp), drop the handled documents from the
    /// background queue so the worker does not redo them, and refresh so
    /// the current query retries them. Fast; refresh failures are fail-open.
    /// Returns the number of documents extracted.
    pub(crate) fn apply_key_phrase_backfill(
        &mut self,
        docs: &[SourceDocument],
        raw: Option<Vec<crate::segments::relations::RawKeyPhrase>>,
    ) -> usize {
        // Apply anything the background worker already finished; those
        // documents are done without needing the extractor output.
        self.drain_enrichment_inbox();
        let raw = match raw {
            Some(raw) => raw,
            None => {
                if let Err(error) = self.flush() {
                    eprintln!("key-phrase backfill refresh failed (fail-open): {error:#}");
                }
                return 0;
            }
        };
        let mut extracted = 0;
        for doc in docs {
            // The extractor ran off-thread without any service lock; a
            // concurrent write may have changed the content since the
            // snapshot — `set_key_phrases` re-validates the hash and skips
            // stale results.
            let hash = crate::pipeline::key_phrase_content_hash(&doc.content);
            let doc_phrases: Vec<crate::source::KeyPhrase> = raw
                .iter()
                .filter(|p| p.doc_id == doc.doc_id)
                .map(|p| crate::source::KeyPhrase {
                    text: p.text.clone(),
                    kind: p.kind.clone(),
                    origin: Some(crate::source::KeyPhraseOrigin::Extractor),
                })
                .collect();
            self.store.set_key_phrases(&doc.doc_id, &hash, doc_phrases);
            // The worker need not redo these documents; only drop the queue
            // entry when it is for the same content this run extracted (a
            // newer write re-queued itself and must be left alone).
            self.enrichment
                .lock()
                .expect("enrichment lock poisoned")
                .pending
                .retain(|p| !(p.turn.doc_id == doc.doc_id && p.content_hash == hash));
            extracted += 1;
        }
        if let Err(error) = self.flush() {
            eprintln!("key-phrase backfill refresh failed (fail-open): {error:#}");
        }
        extracted
    }

    /// Synchronously extract key phrases for documents that still need them
    /// (at most one extractor batch) and refresh the index so the current
    /// query benefits. This is the correctness backstop for documents
    /// written by short-lived processes — provider hooks queue enrichment,
    /// but their worker dies with the process before the linger elapses, so
    /// without this those documents would stay phrase-less until some later
    /// write happened to restart the worker. For the lock-holding server
    /// path use the snapshot/extract/apply split instead, so the subprocess
    /// run does not block concurrent queries.
    ///
    /// Bounded and fail-open: an extractor failure or timeout leaves the
    /// documents phrase-less (a later query retries). Returns the number of
    /// documents extracted.
    pub(crate) fn backfill_key_phrases(&mut self) -> usize {
        let (docs, script) = self.key_phrase_backfill_snapshot();
        if docs.is_empty() {
            return 0;
        }
        let raw = Self::extract_key_phrases_for_docs(&docs, script.as_deref());
        self.apply_key_phrase_backfill(&docs, raw)
    }

    #[cfg(test)]
    pub(crate) fn enrichment_queue_len(&self) -> usize {
        self.enrichment
            .lock()
            .expect("enrichment lock poisoned")
            .pending
            .len()
    }
}

/// Background key-phrase enrichment for newly written documents.
///
/// The write path stays fast: `add` queues the new documents' turns here
/// and returns. A single worker thread drains the queue in batches, runs
/// the extractor's `key_phrases_only` mode off-thread, and posts results
/// to the inbox. The next `&mut` entry point (add/refresh/...) applies the
/// inbox to the store, so the following refresh folds the phrases into the
/// cap-exempt segment entity channel. Everything is fail-open: enrichment
/// never blocks or fails a write, and a missing/slow extractor simply
/// leaves documents with empty key phrases (today's behavior).
#[derive(Default)]
pub(super) struct KeyPhraseEnrichment {
    /// Turns awaiting extraction, in queue order.
    pending: std::collections::VecDeque<PendingTurn>,
    /// Extracted phrases awaiting application to the store.
    pub(super) inbox: Vec<EnrichedDoc>,
    /// True while a worker thread owns the current batch.
    worker_running: bool,
}

/// One queued turn plus the content hash at queue time, so a document that
/// was replaced while its batch was in flight is recognized as stale and
/// its phrases are dropped (the replacement re-queues itself via `add`).
struct PendingTurn {
    turn: crate::segments::relations::RelationTurn,
    content_hash: String,
}

/// Extractor output for one document, ready to backfill.
pub(super) struct EnrichedDoc {
    doc_id: String,
    content_hash: String,
    phrases: Vec<crate::source::KeyPhrase>,
}

/// Max queued turns waiting for extraction; enrichment is best-effort, so
/// overflow drops the oldest queued turns (the documents stay searchable).
pub(super) const KEY_PHRASE_PENDING_MAX: usize = 512;
/// Max turns per extractor subprocess run.
const KEY_PHRASE_BATCH_SIZE: usize = 32;
/// How long the worker lingers before draining, so rapid writes coalesce
/// into one extractor run instead of one model load per write.
const KEY_PHRASE_LINGER_MS: u64 = 500;
/// Bound for one synchronous query-time backfill run: long enough for a
/// warm spaCy model over a batch of short documents, short enough that a
/// cold or stuck extractor cannot stall a query. Fail-open on expiry.
const KEY_PHRASE_BACKFILL_TIMEOUT_SECS: u64 = 30;

/// Run the key-phrase extractor on `turns`, giving up after `timeout_secs`.
/// Returns `None` when the run times out or the worker thread fails
/// `Some(phrases)` when the extractor ran to completion — even when it
/// found nothing, which is then a legitimate empty result — or `None` when
/// the run timed out or the extractor subprocess failed (transient: the
/// documents must stay un-stamped so a later attempt retries). Shared by
/// the background worker and the query-time backfill so both paths bound
/// the subprocess the same way.
pub(super) fn run_key_phrase_extraction_bounded(
    turns: &[crate::segments::relations::RelationTurn],
    script: Option<&std::path::Path>,
    timeout_secs: u64,
) -> Option<Vec<crate::segments::relations::RawKeyPhrase>> {
    let turns: Vec<crate::segments::relations::RelationTurn> = turns.to_vec();
    let script = script.map(|s| s.to_path_buf());
    // Detect language from the combined turn text for model selection.
    let combined_text: String = turns
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let model = crate::lang::default_spacy_model_for_lang(crate::lang::detect_lang(&combined_text));
    let model = model.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = try_extract_key_phrases_via_spacy(
            &turns,
            script.as_deref(),
            std::time::Duration::from_secs(timeout_secs),
            &model,
        );
        let _ = tx.send(out);
    });
    rx.recv_timeout(std::time::Duration::from_secs(timeout_secs))
        .ok()
        .flatten()
}
