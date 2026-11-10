//! Plain text source adapter.
//!
//! Plain text has no headings, and the index chunker cuts sections at heading
//! lines, so a whole `.txt` file would otherwise become one oversized chunk.
//! This adapter gives each file heading-structured content instead: the file
//! name is the top heading, and long files are split at blank lines into
//! bounded parts under `<name> (part N)` headings. Lines that start with `#`
//! are escaped so they are not read as headings.
//!
//! A `.txt` file next to a same-named image (`photo1.txt` beside
//! `photo1.jpg`) is that image's description. The image adapter indexes it,
//! so this adapter skips it.

use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::adapters::markdown::{MarkdownCorpus, SourcePage};
use crate::adapters::{
    image, scan_text_files, scanned_file_tier0_record, AdapterInput, SourceAdapter,
};
use crate::graph::normalize_concept;
use crate::source::SourceDocument;

/// Approximate size of one part of a long text file, in characters. A single
/// line longer than this is kept whole.
const PART_TARGET_CHARS: usize = 2_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct TxtAdapter;

impl SourceAdapter for TxtAdapter {
    fn name(&self) -> &'static str {
        "txt"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(is_txt_extension)
    }

    fn ingest(&self, input: &AdapterInput<'_>) -> Result<Vec<SourceDocument>> {
        let corpus = build_txt_corpus(
            input.root,
            input.max_bytes,
            input.max_files,
            input.max_depth,
            input.max_total_bytes,
        )?;
        Ok(corpus
            .pages
            .into_iter()
            .map(|page| {
                let doc_length = page.content.len();
                SourceDocument {
                    doc_id: page.rel_path.clone(),
                    source: page.rel_path,
                    content: page.content.into(),
                    concept: page.raw_concept,
                    group_id: None,
                    filters: BTreeMap::new(),
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

/// File extensions handled by this adapter.
pub(crate) fn is_txt_extension(extension: &str) -> bool {
    extension == "txt"
}

/// Scan plain text files under `root` into pages and Tier0 records.
///
/// Discovery, limits, and Tier0 metadata follow the Markdown scan so all
/// formats can be merged into one project graph. Plain text has no links.
pub fn build_txt_corpus(
    root: &Path,
    max_bytes: usize,
    max_files: usize,
    max_depth: usize,
    max_total_bytes: usize,
) -> Result<MarkdownCorpus> {
    let mut pages = Vec::new();
    let mut tier0_records = Vec::new();
    let files = scan_text_files(
        root,
        max_bytes,
        max_files,
        max_depth,
        max_total_bytes,
        // A text file next to a same-named image is that image's description
        // and is indexed by the image adapter instead.
        |path, extension| is_txt_extension(extension) && image::described_image(path).is_none(),
    );
    for file in files {
        let page = SourcePage {
            path: file.path.display().to_string(),
            rel_path: file.rel_path.clone(),
            concept: normalize_concept(&file.raw_concept),
            raw_concept: file.raw_concept.clone(),
            content: structure_plain_text(&file.raw_concept, &file.text),
            links: HashSet::new(),
            headings: vec![file.raw_concept.clone()],
        };
        tier0_records.push(scanned_file_tier0_record(&file, &page, None));
        pages.push(page);
    }

    Ok(MarkdownCorpus {
        pages,
        tier0_records,
    })
}

/// Turn plain text into heading-structured content: `# <title>` first, then
/// `## <title> (part N)` before each further part of a long text.
pub(crate) fn structure_plain_text(title: &str, text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut pieces = Vec::new();
    for paragraph in paragraphs(&text) {
        if paragraph.chars().count() <= PART_TARGET_CHARS {
            pieces.push(paragraph);
        } else {
            pieces.extend(split_lines(&paragraph, PART_TARGET_CHARS));
        }
    }

    let mut out = format!("# {title}");
    let mut part = 1usize;
    let mut part_chars = 0usize;
    for piece in pieces {
        let piece_chars = piece.chars().count();
        if part_chars > 0 && part_chars + piece_chars > PART_TARGET_CHARS {
            part += 1;
            part_chars = 0;
            out.push_str(&format!("\n\n## {title} (part {part})"));
        }
        out.push_str("\n\n");
        out.push_str(&piece);
        part_chars += piece_chars;
    }
    out
}

/// Blank-line separated paragraphs, with heading-like lines escaped.
fn paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n"));
                current.clear();
            }
            continue;
        }
        let line = line.trim_end();
        if line.starts_with('#') {
            current.push(format!("\\{line}"));
        } else {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n"));
    }
    out
}

/// Split one long paragraph into groups of whole lines near `target` chars.
fn split_lines(paragraph: &str, target: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in paragraph.lines() {
        if !current.is_empty() && current.chars().count() + line.chars().count() + 1 > target {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn short_text_is_one_section_under_the_file_name() {
        let content = structure_plain_text("notes", "First line\r\nsecond line\n\n\n\nNext paragraph.\n");
        assert_eq!(content, "# notes\n\nFirst line\nsecond line\n\nNext paragraph.");
    }

    #[test]
    fn empty_or_blank_text_keeps_only_the_file_name_heading() {
        assert_eq!(structure_plain_text("empty", ""), "# empty");
        assert_eq!(structure_plain_text("blank", "\n  \n\t\n"), "# blank");
    }

    #[test]
    fn lines_starting_with_hash_are_escaped() {
        let content = structure_plain_text("log", "# not a heading\n## also not\nplain");
        assert_eq!(content, "# log\n\n\\# not a heading\n\\## also not\nplain");
    }

    #[test]
    fn long_text_is_split_into_bounded_parts_at_paragraphs() {
        let paragraph = "word ".repeat(150).trim_end().to_string(); // 749 chars
        let text = vec![paragraph.clone(); 5].join("\n\n");
        let content = structure_plain_text("big", &text);

        let parts: Vec<_> = content.split("\n\n## big (part ").collect();
        assert_eq!(parts.len(), 3, "{content}");
        assert!(parts[0].starts_with("# big\n\n"));
        for part in &parts {
            assert!(part.chars().count() <= PART_TARGET_CHARS + 32, "part too long");
        }
        assert_eq!(content.matches(&paragraph).count(), 5);
    }

    #[test]
    fn one_huge_paragraph_is_split_at_line_boundaries() {
        let line = "x".repeat(300);
        let text = vec![line.clone(); 20].join("\n");
        let content = structure_plain_text("dump", &text);
        assert!(content.contains("## dump (part 2)"));
        assert_eq!(content.matches(&line).count(), 20);
    }

    #[test]
    fn corpus_scans_only_txt_files() -> Result<()> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("membrick-txt-corpus-{nanos}"));
        fs::create_dir_all(&root)?;
        fs::write(root.join("meeting-notes.txt"), "Decided to ship 0.8.3.\n")?;
        fs::write(root.join("readme.md"), "# Readme\n")?;

        let result = build_txt_corpus(&root, 1 << 20, 10, 10, 1 << 20);
        fs::remove_dir_all(&root)?;
        let corpus = result?;

        assert_eq!(corpus.pages.len(), 1);
        let page = &corpus.pages[0];
        assert_eq!(page.rel_path, "meeting-notes.txt");
        assert_eq!(page.concept, "meeting notes");
        assert_eq!(page.content, "# meeting-notes\n\nDecided to ship 0.8.3.");
        assert_eq!(page.headings, vec!["meeting-notes"]);
        assert_eq!(
            corpus.tier0_records[0].metadata.get("file_ext").map(String::as_str),
            Some("txt")
        );
        Ok(())
    }
}
