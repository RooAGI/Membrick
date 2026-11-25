//! Reusing a stored lexical index instead of rebuilding it.
//!
//! The records are the truth; a segment's folder on disk is a copy of them.
//! Its path names only the segment, and a write refreshes the index in RAM
//! without touching the folder, so a folder may hold older documents. A
//! folder is therefore reused only when its last commit carries the
//! fingerprint of exactly what a rebuild would write now.
//!
//! The fingerprint is the Tantivy commit payload: it is saved in the same
//! atomic `meta.json` replacement as the documents, so an interrupted build
//! leaves the previous commit with the previous fingerprint, and any commit
//! that does not set one clears it.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tantivy::schema::Schema;
use tantivy::tokenizer::TokenStream;
use tantivy::{Index, ReloadPolicy};

use super::model::DocRecord;

/// Bump when the same texts and schema would be indexed differently in a way
/// the tokenizer probe below cannot see.
const LEXICAL_INDEX_VERSION: &str = "membrick-lexical-index-1";

/// Text in every script the tokenizer treats differently. Its tokens are part
/// of the fingerprint, so a tokenizer change rebuilds stored indexes. Editing
/// the text does too.
const TOKENIZER_PROBE: &str =
    "Rebuilt weekends x1.2.3 snake_case kebab-case niño İstanbul 我毕业于清华大学 학교에 갔다 a1";

/// The texts of one document, exactly as they are written to the index. The
/// writer and the fingerprint share it so they cannot drift apart.
pub(super) struct LexicalDocTexts {
    pub(super) doc_id: String,
    pub(super) content: String,
    pub(super) headings: String,
    pub(super) terms: String,
    pub(super) entities: String,
    pub(super) temporal: String,
    pub(super) tags: String,
}

impl LexicalDocTexts {
    pub(super) fn new(doc: &DocRecord, tags: &[String]) -> Self {
        let chunks = &doc.section_chunks;
        Self {
            doc_id: doc.doc_id.clone(),
            content: super::build::lexical_content_text(doc),
            headings: chunks
                .iter()
                .map(|chunk| chunk.heading.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            terms: chunks
                .iter()
                .flat_map(|chunk| chunk.important_terms.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            entities: chunks
                .iter()
                .flat_map(|chunk| chunk.key_entities.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            // Tags stay in their own field: categories ("leisure", "travel")
            // in the content would let unrelated documents match.
            temporal: doc.temporal_terms.join(" "),
            tags: tags.join(" "),
        }
    }
}

/// Identifies everything a rebuild would write: the index format, the schema,
/// the tokenizers, and every document's texts in order.
pub(super) fn lexical_fingerprint(
    schema: &Schema,
    ordered: &[&DocRecord],
    tags_per_doc: &[Vec<String>],
) -> String {
    let mut hasher = Sha256::new();
    let mut add = |text: &str| {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    };
    add(LEXICAL_INDEX_VERSION);
    add(tantivy::version_string());
    add(&serde_json::to_string(schema).unwrap_or_default());
    add(tokenizer_identity());
    for (doc, tags) in ordered.iter().zip(tags_per_doc) {
        let texts = LexicalDocTexts::new(doc, tags);
        for text in [
            &texts.doc_id,
            &texts.content,
            &texts.headings,
            &texts.terms,
            &texts.entities,
            &texts.temporal,
            &texts.tags,
        ] {
            add(text);
        }
    }
    format!("{:x}", hasher.finalize())
}

/// The registered tokenizers' tokens for the probe text.
fn tokenizer_identity() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let index = Index::create_in_ram(Schema::builder().build());
        super::cjk_tokenizer::register_cjk_tokenizer(&index);
        let mut identity = String::new();
        for name in ["default", "subword"] {
            let Some(mut tokenizer) = index.tokenizers().get(name) else {
                continue;
            };
            let mut stream = tokenizer.token_stream(TOKENIZER_PROBE);
            stream.process(&mut |token| {
                identity.push_str(&format!("{name}:{}:{}\n", token.position, token.text));
            });
        }
        identity
    })
}

/// Why a stored index cannot be reused, or `None` when it can.
pub(super) fn reuse_refusal(index: &Index, fingerprint: &str, documents: usize) -> Option<String> {
    let metas = match index.load_metas() {
        Ok(metas) => metas,
        Err(error) => return Some(format!("its metadata is unreadable: {error}")),
    };
    match metas.payload.as_deref() {
        None => return Some("it has no fingerprint".to_string()),
        Some(stored) if stored != fingerprint => {
            return Some("its contents differ from the records".to_string())
        }
        Some(_) => {}
    }
    // A missing file fails here; a damaged one fails its checksum below.
    let reader = match index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
    {
        Ok(reader) => reader,
        Err(error) => return Some(format!("it does not open: {error}")),
    };
    let stored = reader.searcher().num_docs();
    if stored != documents as u64 {
        return Some(format!("it holds {stored} documents, not {documents}"));
    }
    match index.validate_checksum() {
        Ok(damaged) if damaged.is_empty() => None,
        Ok(damaged) => Some(format!("{} of its files are damaged", damaged.len())),
        Err(error) => Some(format!("its files are unreadable: {error}")),
    }
}
