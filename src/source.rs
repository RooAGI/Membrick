use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::stable_doc_id_from_source;

const TIMESTAMP_KIND_FILTER: &str = "timestamp_kind";

/// Describes what a source document's timestamp means. The serialized value
/// lives in the existing document filters so persisted records remain
/// backward-compatible and provider adapters keep using `SourceDocument`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampKind {
    Session,
    SourceModified,
    WorkspaceEvent,
    UserSupplied,
    Unknown,
}

impl TimestampKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::SourceModified => "source_modified",
            Self::WorkspaceEvent => "workspace_event",
            Self::UserSupplied => "user_supplied",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            "source_modified" => Some(Self::SourceModified),
            "workspace_event" => Some(Self::WorkspaceEvent),
            "user_supplied" => Some(Self::UserSupplied),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDocument {
    pub doc_id: String,
    pub source: String,
    /// Shared with the record built from this document, so the store keeps
    /// one copy of the text.
    pub content: crate::index::SharedText,
    pub concept: String,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub headings: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    pub timestamp: Option<String>,
    pub doc_length: usize,
    pub author_agent: Option<String>,
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    /// Grammar-accepted entity mentions for this document's session
    /// (behood noun-phrase layer, e.g. "Harry Potter conference"). Carried
    /// into the segment summary's entity channel, which is exempt from the
    /// local-memory term cap, so discriminative phrases survive routing.
    #[serde(default)]
    pub key_phrases: Vec<KeyPhrase>,
    /// Content hash the key phrases were extracted from. Empty means
    /// extraction has not completed for the current content yet; matching
    /// `key_phrase_content_hash(content)` means it has (even when
    /// `key_phrases` is empty, which is then a legitimate empty result).
    /// A mismatch means the content changed after extraction and the
    /// phrases must be recomputed.
    #[serde(default)]
    pub key_phrase_extraction_hash: String,
}

/// One grammar-accepted entity mention: a noun phrase the dependency
/// grammar judged name-worthy (proper-noun participants, not a person or
/// pronoun head), with behood's ontological kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyPhrase {
    /// Literal phrase text, e.g. "Harry Potter conference".
    pub text: String,
    /// Ontological kind: "event" | "place" | "org" | "work" | "thing".
    pub kind: String,
    /// Identifies the writer of this annotation so extractor refreshes can
    /// replace their own output without deleting provider-approved phrases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<KeyPhraseOrigin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPhraseOrigin {
    Extractor,
    ProviderEnrichment,
    SemanticTag,
}

/// Additive changes proposed for an existing searchable source document.
/// Callers identify the document by its stable ID and the exact source
/// revision they reasoned about; MemoryService performs validation and merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDocumentEnrichment {
    pub doc_id: String,
    pub expected_source_hash: String,
    #[serde(default)]
    pub key_phrases: Vec<KeyPhrase>,
    /// Replace the managed conclusion on an existing session-context source.
    /// The original observations remain separate linked source documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_conclusion: Option<String>,
    /// Add searchable headings to an existing generated session context.
    #[serde(default)]
    pub headings: Vec<String>,
    /// Add links to existing source documents cited by this enrichment.
    #[serde(default)]
    pub links: Vec<String>,
    /// Explicitly mark this document as replacing another reviewed document.
    /// MemoryService stores this using the existing `supersedes_id` source
    /// metadata consumed by the semantic relation engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_document_id: Option<String>,
    /// Existing source documents from the prepared operation that support
    /// these additions. MCP submissions must cite at least one such source.
    #[serde(default)]
    pub evidence: Vec<SourceDocumentEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDocumentEvidence {
    pub doc_id: String,
    pub expected_source_hash: String,
}

/// Deterministic optimistic-concurrency token for one complete source record.
pub fn source_document_hash(document: &SourceDocument) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_vec(document)?;
    let digest = Sha256::digest(encoded);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl SourceDocument {
    /// Normalize timestamp units and annotate legacy documents with the
    /// timestamp meaning that can be inferred from their existing provenance.
    pub fn normalize_timestamp_metadata(&mut self) {
        if let Some(value) = self.timestamp.as_deref() {
            if let Some(normalized) = crate::temporal::normalize_timestamp(value) {
                self.timestamp = Some(normalized);
            }
        }
        if !self.filters.contains_key(TIMESTAMP_KIND_FILTER) {
            let kind = self.inferred_timestamp_kind();
            if kind != TimestampKind::Unknown {
                self.filters
                    .insert(TIMESTAMP_KIND_FILTER.to_string(), kind.as_str().to_string());
            }
        }
    }

    pub fn timestamp_kind(&self) -> TimestampKind {
        self.filters
            .get(TIMESTAMP_KIND_FILTER)
            .and_then(|value| TimestampKind::parse(value))
            .unwrap_or_else(|| self.inferred_timestamp_kind())
    }

    fn inferred_timestamp_kind(&self) -> TimestampKind {
        if self
            .filters
            .get("source_type")
            .is_some_and(|value| value == "workspace-file-change")
        {
            return TimestampKind::WorkspaceEvent;
        }
        if self.source.starts_with("memory://") {
            return TimestampKind::UserSupplied;
        }
        if crate::provider_sources::is_provider_session(&self.source, self.group_id.as_deref())
            || (self.source.starts_with("membrick://") && self.source.contains("/session/"))
            || (self.source.starts_with("lint-ai://")
                && self
                    .group_id
                    .as_deref()
                    .is_some_and(|group| group.contains("session")))
        {
            return TimestampKind::Session;
        }
        TimestampKind::Unknown
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_stable_doc_id_from_source(
        source: String,
        content: String,
        concept: String,
        group_id: Option<String>,
        headings: Vec<String>,
        links: Vec<String>,
        timestamp: Option<String>,
        author_agent: Option<String>,
    ) -> Self {
        let doc_id = stable_doc_id_from_source(&source);
        let doc_length = content.len();
        Self {
            doc_id,
            source,
            content: content.into(),
            concept,
            group_id,
            headings,
            links,
            timestamp,
            doc_length,
            author_agent,
            filters: BTreeMap::new(),
            key_phrases: Vec::new(),
            key_phrase_extraction_hash: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_stable_doc_id_sets_doc_length_from_content() {
        let doc = SourceDocument::with_stable_doc_id_from_source(
            "docs/install.md".to_string(),
            "install guide for linux hosts".to_string(),
            "install guide".to_string(),
            None,
            vec!["Overview".to_string()],
            vec!["docs/setup.md".to_string()],
            Some("2026-05-29".to_string()),
            Some("agent".to_string()),
        );

        assert_eq!(doc.doc_length, "install guide for linux hosts".len());
        assert_eq!(doc.source, "docs/install.md");
        assert_eq!(doc.concept, "install guide");
        assert_eq!(doc.headings, vec!["Overview".to_string()]);
        assert_eq!(doc.links, vec!["docs/setup.md".to_string()]);
        assert_eq!(doc.author_agent.as_deref(), Some("agent"));
        assert_eq!(doc.timestamp.as_deref(), Some("2026-05-29"));
        assert!(!doc.doc_id.is_empty());
    }

    #[test]
    fn with_stable_doc_id_is_deterministic_for_same_source() {
        let first = SourceDocument::with_stable_doc_id_from_source(
            "docs/install.md".to_string(),
            "content one".to_string(),
            "install guide".to_string(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        let second = SourceDocument::with_stable_doc_id_from_source(
            "docs/install.md".to_string(),
            "different content".to_string(),
            "install guide".to_string(),
            None,
            vec![],
            vec![],
            None,
            None,
        );

        assert_eq!(first.doc_id, second.doc_id);
    }

    #[test]
    fn source_document_round_trips_through_json() {
        let doc = SourceDocument::with_stable_doc_id_from_source(
            "docs/install.md".to_string(),
            "install guide for linux hosts".to_string(),
            "install guide".to_string(),
            Some("group-1".to_string()),
            vec!["Overview".to_string()],
            vec!["docs/setup.md".to_string()],
            Some("2026-05-29".to_string()),
            Some("agent".to_string()),
        );

        let json = serde_json::to_string(&doc).unwrap();
        let decoded: SourceDocument = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.doc_id, doc.doc_id);
        assert_eq!(decoded.source, doc.source);
        assert_eq!(decoded.content, doc.content);
        assert_eq!(decoded.concept, doc.concept);
        assert_eq!(decoded.group_id, doc.group_id);
        assert_eq!(decoded.headings, doc.headings);
        assert_eq!(decoded.links, doc.links);
        assert_eq!(decoded.timestamp, doc.timestamp);
        assert_eq!(decoded.doc_length, doc.doc_length);
        assert_eq!(decoded.author_agent, doc.author_agent);
        assert_eq!(decoded.filters, doc.filters);
    }

    #[test]
    fn timestamp_normalization_preserves_meaning_and_rejects_ambiguous_years() {
        let mut session = SourceDocument::with_stable_doc_id_from_source(
            "codex://project/session/s1/outcome".into(),
            "Outcome".into(),
            "session outcome".into(),
            Some("codex-session:s1".into()),
            vec![],
            vec![],
            Some("1792220974000".into()),
            None,
        );
        session.normalize_timestamp_metadata();
        assert_eq!(session.timestamp.as_deref(), Some("2026-10-17T07:09:34Z"));
        assert_eq!(session.timestamp_kind(), TimestampKind::Session);

        let mut file = SourceDocument::with_stable_doc_id_from_source(
            "docs/guide.md".into(),
            "Guide".into(),
            "guide".into(),
            None,
            vec![],
            vec![],
            Some("2026".into()),
            None,
        );
        file.filters.insert(
            TIMESTAMP_KIND_FILTER.into(),
            TimestampKind::SourceModified.as_str().into(),
        );
        file.normalize_timestamp_metadata();
        assert_eq!(file.timestamp.as_deref(), Some("2026"));
        assert_eq!(file.timestamp_kind(), TimestampKind::SourceModified);
    }

    #[test]
    fn legacy_key_phrases_without_origin_still_deserialize() {
        let mut doc = SourceDocument::with_stable_doc_id_from_source(
            "docs/legacy.md".to_string(),
            "A legacy source document.".to_string(),
            "legacy document".to_string(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        doc.key_phrases = vec![KeyPhrase {
            text: "legacy source".to_string(),
            kind: "thing".to_string(),
            origin: Some(KeyPhraseOrigin::ProviderEnrichment),
        }];

        let mut value = serde_json::to_value(&doc).unwrap();
        value["key_phrases"][0]
            .as_object_mut()
            .unwrap()
            .remove("origin");
        let decoded: SourceDocument = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.key_phrases.len(), 1);
        assert_eq!(decoded.key_phrases[0].text, "legacy source");
        assert_eq!(decoded.key_phrases[0].kind, "thing");
        assert_eq!(decoded.key_phrases[0].origin, None);
    }
}
