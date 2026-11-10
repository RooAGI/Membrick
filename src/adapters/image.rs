//! Image source adapter.
//!
//! Membrick does not look inside image files. An image is indexed through its
//! description: a `.txt` file with the same name in the same directory
//! (`photo1.txt` describes `photo1.jpg`). The image and its description
//! become one document whose id and source are the image path, so search
//! results point at the image. The description file is not indexed again by
//! the plain text adapter. Images without a description are skipped.

use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::adapters::markdown::{MarkdownCorpus, SourcePage};
use crate::adapters::txt::structure_plain_text;
use crate::adapters::{
    scan_files_with_text, scanned_file_tier0_record, AdapterInput, SourceAdapter,
};
use crate::graph::normalize_concept;
use crate::source::SourceDocument;

/// Image extensions this adapter accepts, in lookup order. Matching is case
/// insensitive because cameras commonly write `.JPG`.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];

/// Extension of an image's description file.
const DESCRIPTION_EXTENSION: &str = "txt";

#[derive(Debug, Default, Clone, Copy)]
pub struct ImageAdapter;

impl SourceAdapter for ImageAdapter {
    fn name(&self) -> &'static str {
        "image"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(is_image_extension)
    }

    fn ingest(&self, input: &AdapterInput<'_>) -> Result<Vec<SourceDocument>> {
        let corpus = build_image_corpus(
            input.root,
            input.max_bytes,
            input.max_files,
            input.max_depth,
            input.max_total_bytes,
        )?;
        Ok(corpus
            .pages
            .into_iter()
            .zip(corpus.tier0_records)
            .map(|(page, record)| {
                let doc_length = page.content.len();
                let mut filters = BTreeMap::new();
                if let Some(media_type) = record.metadata.get("media_type") {
                    filters.insert("media_type".to_string(), media_type.clone());
                }
                SourceDocument {
                    doc_id: page.rel_path.clone(),
                    source: page.rel_path,
                    content: page.content.into(),
                    concept: page.raw_concept,
                    group_id: None,
                    filters,
                    headings: page.headings,
                    links: Vec::new(),
                    timestamp: None,
                    doc_length,
                    author_agent: None,
                    key_phrases: Vec::new(),
                    key_phrase_extraction_hash: String::new(),
                }
            })
            .collect())
    }
}

/// Whether an extension names a supported image format.
pub(crate) fn is_image_extension(extension: &str) -> bool {
    IMAGE_EXTENSIONS
        .iter()
        .any(|image| extension.eq_ignore_ascii_case(image))
}

/// The image a `.txt` file describes, when a same-named image sits next to it.
pub(crate) fn described_image(text_path: &Path) -> Option<PathBuf> {
    IMAGE_EXTENSIONS
        .iter()
        .flat_map(|extension| [extension.to_string(), extension.to_ascii_uppercase()])
        .map(|extension| text_path.with_extension(extension))
        .find(|candidate| candidate.is_file())
}

/// The description file for an image, when one exists.
fn description_path(image_path: &Path) -> Option<PathBuf> {
    let candidate = image_path.with_extension(DESCRIPTION_EXTENSION);
    candidate.is_file().then_some(candidate)
}

fn media_type(extension: &str) -> &'static str {
    if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else {
        "image/jpeg"
    }
}

/// Scan images that have a description file under `root` into pages and
/// Tier0 records. Byte limits apply to the descriptions, not the images.
pub fn build_image_corpus(
    root: &Path,
    max_bytes: usize,
    max_files: usize,
    max_depth: usize,
    max_total_bytes: usize,
) -> Result<MarkdownCorpus> {
    let mut pages = Vec::new();
    let mut tier0_records = Vec::new();
    let files = scan_files_with_text(
        root,
        max_bytes,
        max_files,
        max_depth,
        max_total_bytes,
        |_, extension| is_image_extension(extension),
        description_path,
    );
    for file in files {
        let file_name = file
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let page = SourcePage {
            path: file.path.display().to_string(),
            rel_path: file.rel_path.clone(),
            concept: normalize_concept(&file.raw_concept),
            raw_concept: file.raw_concept.clone(),
            content: structure_plain_text(
                &file.raw_concept,
                &format!("Image: {file_name}\n\n{}", file.text),
            ),
            links: HashSet::new(),
            headings: vec![file.raw_concept.clone()],
        };
        let mut record = scanned_file_tier0_record(&file, &page, None);
        record
            .metadata
            .insert("media_type".to_string(), media_type(&file.extension).to_string());
        record.metadata.insert(
            "description_path".to_string(),
            Path::new(&file.rel_path)
                .with_extension(DESCRIPTION_EXTENSION)
                .display()
                .to_string(),
        );
        tier0_records.push(record);
        pages.push(page);
    }

    Ok(MarkdownCorpus {
        pages,
        tier0_records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::txt::build_txt_corpus;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> Result<PathBuf> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("membrick-image-{name}-{nanos}"));
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn image_extensions_match_case_insensitively() {
        assert!(is_image_extension("jpg"));
        assert!(is_image_extension("JPG"));
        assert!(is_image_extension("jpeg"));
        assert!(is_image_extension("png"));
        assert!(!is_image_extension("gif"));
        assert!(!is_image_extension("txt"));
    }

    #[test]
    fn described_images_become_one_document_and_their_text_is_not_reindexed() -> Result<()> {
        let root = temp_root("pairs")?;
        fs::create_dir_all(root.join("trip"))?;
        // Bytes that are not valid UTF-8 prove the image itself is never read.
        fs::write(root.join("trip/photo1.jpg"), [0xFF, 0xD8, 0xFF, 0xE0])?;
        fs::write(root.join("trip/photo1.txt"), "Sunset over the harbor.\n")?;
        fs::write(root.join("diagram.PNG"), [0x89, 0x50, 0x4E, 0x47])?;
        fs::write(root.join("diagram.txt"), "Architecture overview.")?;
        fs::write(root.join("no-description.png"), [0x89, 0x50])?;
        fs::write(root.join("notes.txt"), "Plain notes.")?;

        let images = build_image_corpus(&root, 1 << 20, 10, 10, 1 << 20);
        let texts = build_txt_corpus(&root, 1 << 20, 10, 10, 1 << 20);
        fs::remove_dir_all(&root)?;
        let images = images?;
        let texts = texts?;

        let mut image_paths: Vec<_> = images.pages.iter().map(|p| p.rel_path.as_str()).collect();
        image_paths.sort();
        assert_eq!(image_paths, vec!["diagram.PNG", "trip/photo1.jpg"]);

        let photo = images
            .pages
            .iter()
            .find(|p| p.rel_path == "trip/photo1.jpg")
            .expect("photo page");
        assert_eq!(
            photo.content,
            "# photo1\n\nImage: photo1.jpg\n\nSunset over the harbor."
        );
        let record = images
            .tier0_records
            .iter()
            .find(|r| r.source == "trip/photo1.jpg")
            .expect("photo record");
        assert_eq!(record.metadata.get("media_type").map(String::as_str), Some("image/jpeg"));
        assert_eq!(
            record.metadata.get("description_path").map(String::as_str),
            Some("trip/photo1.txt")
        );
        assert_eq!(record.doc_length, "Sunset over the harbor.\n".len());

        let text_paths: Vec<_> = texts.pages.iter().map(|p| p.rel_path.as_str()).collect();
        assert_eq!(text_paths, vec!["notes.txt"]);
        Ok(())
    }

    #[test]
    fn large_images_are_not_skipped_by_the_byte_limit() -> Result<()> {
        let root = temp_root("large")?;
        fs::write(root.join("big.png"), vec![0u8; 4096])?;
        fs::write(root.join("big.txt"), "A large picture.")?;

        let result = build_image_corpus(&root, 1024, 10, 10, 1024);
        fs::remove_dir_all(&root)?;
        assert_eq!(result?.pages.len(), 1);
        Ok(())
    }

    #[test]
    fn an_oversized_description_skips_its_image() -> Result<()> {
        let root = temp_root("oversized")?;
        fs::write(root.join("small.png"), [0x89])?;
        fs::write(root.join("small.txt"), "Short.")?;
        fs::write(root.join("wordy.png"), [0x89])?;
        fs::write(root.join("wordy.txt"), "w".repeat(2048))?;

        let result = build_image_corpus(&root, 1024, 10, 10, 1 << 20);
        fs::remove_dir_all(&root)?;
        let paths: Vec<_> = result?.pages.into_iter().map(|p| p.rel_path).collect();
        assert_eq!(paths, vec!["small.png"]);
        Ok(())
    }
}
