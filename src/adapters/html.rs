//! HTML source adapter.
//!
//! Each `.html` or `.htm` file is converted into heading-structured text that
//! uses Markdown heading lines (`# Title`) and blank-line separated blocks.
//! Section chunking, heading extraction, and link resolution downstream then
//! treat an HTML page the same way as a Markdown page. Only readable content
//! is kept: scripts, styles, and other non-text elements are dropped.

use anyhow::Result;
use scraper::{ElementRef, Html, Node, Selector};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::adapters::markdown::{MarkdownCorpus, SourcePage};
use crate::adapters::{
    scan_text_files, scanned_file_tier0_record, AdapterInput, SourceAdapter,
};
use crate::graph::{concept_from_link_target, normalize_concept};
use crate::source::SourceDocument;

#[derive(Debug, Default, Clone, Copy)]
pub struct HtmlAdapter;

impl SourceAdapter for HtmlAdapter {
    fn name(&self) -> &'static str {
        "html"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(is_html_extension)
    }

    fn ingest(&self, input: &AdapterInput<'_>) -> Result<Vec<SourceDocument>> {
        let corpus = build_html_corpus(
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
                    links: page.links.into_iter().collect(),
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
pub(crate) fn is_html_extension(extension: &str) -> bool {
    extension == "html" || extension == "htm"
}

/// Scan HTML files under `root` into pages and Tier0 records.
///
/// Discovery, limits, and Tier0 metadata follow the Markdown scan so both
/// formats can be merged into one project graph.
pub fn build_html_corpus(
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
        |_, extension| is_html_extension(extension),
    );
    for file in files {
        let html = parse_html_page(&file.text);
        let links: HashSet<String> = html
            .link_targets
            .iter()
            .filter_map(|target| concept_from_link_target(target))
            .collect();
        let page = SourcePage {
            path: file.path.display().to_string(),
            rel_path: file.rel_path.clone(),
            concept: normalize_concept(&file.raw_concept),
            raw_concept: file.raw_concept.clone(),
            content: html.content,
            links,
            headings: html.headings,
        };
        tier0_records.push(scanned_file_tier0_record(&file, &page, html.author));
        pages.push(page);
    }

    Ok(MarkdownCorpus {
        pages,
        tier0_records,
    })
}

/// Readable content extracted from one HTML document.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HtmlPage {
    /// Heading-structured text with blank lines between blocks.
    pub content: String,
    /// Heading texts in document order.
    pub headings: Vec<String>,
    /// Raw `href` values of anchors, for link resolution by the caller.
    pub link_targets: Vec<String>,
    /// Value of `<meta name="author">`, when present.
    pub author: Option<String>,
}

/// Elements whose content is never readable text.
const SKIPPED_ELEMENTS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object",
];

/// Elements that start and end their own block of text.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "tr",
    "ul",
];

/// Nesting depth past which the remaining subtree is flattened to plain text,
/// so pathological documents cannot exhaust the stack.
const MAX_DEPTH: usize = 256;

/// Parse an HTML document into readable, heading-structured text.
pub(crate) fn parse_html_page(source: &str) -> HtmlPage {
    let document = Html::parse_document(source);
    let mut writer = BlockWriter::default();
    let mut page = HtmlPage {
        author: meta_author(&document),
        ..HtmlPage::default()
    };

    let has_h1 = Selector::parse("h1")
        .ok()
        .is_some_and(|selector| document.select(&selector).next().is_some());
    if !has_h1 {
        if let Some(title) = document_title(&document) {
            writer.push_heading(1, &title);
            page.headings.push(title);
        }
    }

    walk(document.root_element(), 0, &mut writer, &mut page);
    writer.flush();
    page.content = writer.out;
    page
}

fn walk(element: ElementRef<'_>, depth: usize, writer: &mut BlockWriter, page: &mut HtmlPage) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => writer.push_text(text),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    visit_element(child, depth + 1, writer, page);
                }
            }
            _ => {}
        }
    }
}

fn visit_element(
    element: ElementRef<'_>,
    depth: usize,
    writer: &mut BlockWriter,
    page: &mut HtmlPage,
) {
    let name = element.value().name();
    if SKIPPED_ELEMENTS.contains(&name) {
        return;
    }
    if depth > MAX_DEPTH {
        writer.push_text(&element.text().collect::<String>());
        return;
    }
    if let Some(level) = heading_level(name) {
        let text = collapse_whitespace(&element.text().collect::<String>());
        if !text.is_empty() {
            writer.push_heading(level, &text);
            page.headings.push(text);
        }
        return;
    }
    match name {
        "br" => writer.flush(),
        "pre" => writer.push_preformatted(&element.text().collect::<String>()),
        "img" => {
            if let Some(alt) = element.value().attr("alt") {
                writer.push_text(alt);
            }
        }
        "td" | "th" => {
            writer.push_cell_separator();
            walk(element, depth, writer, page);
        }
        "a" => {
            if let Some(href) = element.value().attr("href") {
                page.link_targets.push(href.trim().to_string());
            }
            walk(element, depth, writer, page);
        }
        _ if BLOCK_ELEMENTS.contains(&name) => {
            writer.flush();
            if name == "li" {
                writer.prefix = "- ";
            }
            walk(element, depth, writer, page);
            writer.flush();
        }
        _ => walk(element, depth, writer, page),
    }
}

fn heading_level(name: &str) -> Option<usize> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn document_title(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    let title = collapse_whitespace(&document.select(&selector).next()?.text().collect::<String>());
    (!title.is_empty()).then_some(title)
}

fn meta_author(document: &Html) -> Option<String> {
    let selector = Selector::parse("meta[name]").ok()?;
    document
        .select(&selector)
        .find(|meta| {
            meta.value()
                .attr("name")
                .is_some_and(|name| name.eq_ignore_ascii_case("author"))
        })
        .and_then(|meta| meta.value().attr("content"))
        .map(collapse_whitespace)
        .filter(|author| !author.is_empty())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Accumulates inline text into blank-line separated blocks.
#[derive(Default)]
struct BlockWriter {
    out: String,
    line: String,
    prefix: &'static str,
    pending_space: bool,
}

impl BlockWriter {
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if text.starts_with(char::is_whitespace) {
            self.pending_space = true;
        }
        for word in text.split_whitespace() {
            if self.pending_space && !self.line.is_empty() {
                self.line.push(' ');
            }
            self.line.push_str(word);
            self.pending_space = true;
        }
        self.pending_space = text.ends_with(char::is_whitespace);
    }

    fn push_cell_separator(&mut self) {
        if !self.line.is_empty() {
            self.line.push_str(" | ");
        }
        self.pending_space = false;
    }

    fn push_heading(&mut self, level: usize, text: &str) {
        self.flush();
        self.push_block(&format!("{} {}", "#".repeat(level), text));
    }

    fn push_preformatted(&mut self, text: &str) {
        self.flush();
        let text = text.trim_matches('\n');
        if !text.trim().is_empty() {
            self.push_block(&format!("```\n{text}\n```"));
        }
    }

    fn flush(&mut self) {
        let line = std::mem::take(&mut self.line);
        let prefix = std::mem::take(&mut self.prefix);
        self.pending_space = false;
        let text = line.trim();
        if text.is_empty() {
            return;
        }
        // A body line that starts with `#` would read as a heading to the
        // section chunker; escape it the way Markdown does.
        let block = if prefix.is_empty() && text.starts_with('#') {
            format!("\\{text}")
        } else {
            format!("{prefix}{text}")
        };
        self.push_block(&block);
    }

    fn push_block(&mut self, block: &str) {
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        self.out.push_str(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn headings_paragraphs_and_lists_become_structured_text() {
        let page = parse_html_page(
            "<html><head><title>Ignored</title></head><body>\
             <h1>Guide</h1><p>Intro <b>bold</b>text and <i>more</i>.</p>\
             <h2>Steps</h2><ul><li>First</li><li>Second <a href=\"setup.html#run\">setup</a></li></ul>\
             </body></html>",
        );
        assert_eq!(
            page.content,
            "# Guide\n\nIntro boldtext and more.\n\n## Steps\n\n- First\n\n- Second setup"
        );
        assert_eq!(page.headings, vec!["Guide", "Steps"]);
        assert_eq!(page.link_targets, vec!["setup.html#run"]);
    }

    #[test]
    fn scripts_styles_and_head_are_dropped() {
        let page = parse_html_page(
            "<html><head><style>p{}</style><script>var x = 1;</script></head>\
             <body><h1>Doc</h1><script>alert(1)</script><noscript>off</noscript><p>Kept</p></body></html>",
        );
        assert_eq!(page.content, "# Doc\n\nKept");
    }

    #[test]
    fn title_becomes_top_heading_when_page_has_no_h1() {
        let page = parse_html_page(
            "<html><head><title> Release  notes </title></head><body><h2>0.8</h2><p>Fixes.</p></body></html>",
        );
        assert_eq!(page.content, "# Release notes\n\n## 0.8\n\nFixes.");
        assert_eq!(page.headings, vec!["Release notes", "0.8"]);
    }

    #[test]
    fn tables_preformatted_text_and_entities_are_kept_readable() {
        let page = parse_html_page(
            "<body><table><tr><th>Key</th><th>Value</th></tr><tr><td>a &amp; b</td><td>1</td></tr></table>\
             <pre>fn main() {\n    run();\n}</pre><p><img src=\"x.png\" alt=\"Diagram\"> caption</p></body>",
        );
        assert_eq!(
            page.content,
            "Key | Value\n\na & b | 1\n\n```\nfn main() {\n    run();\n}\n```\n\nDiagram caption"
        );
    }

    #[test]
    fn body_text_starting_with_hash_is_not_read_as_heading() {
        let page = parse_html_page("<body><p># not a heading</p></body>");
        assert_eq!(page.content, "\\# not a heading");
        assert!(page.headings.is_empty());
    }

    #[test]
    fn meta_author_is_extracted() {
        let page = parse_html_page(
            "<html><head><meta name=\"Author\" content=\" Ada \"></head><body><p>x</p></body></html>",
        );
        assert_eq!(page.author.as_deref(), Some("Ada"));
    }

    #[test]
    fn deeply_nested_markup_keeps_its_text_without_overflowing_the_stack() {
        let depth = MAX_DEPTH * 4;
        let source = format!(
            "<body>{}<span>deep text</span>{}</body>",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        let page = parse_html_page(&source);
        assert_eq!(page.content, "deep text");
    }

    #[test]
    fn nested_lists_flatten_into_one_item_per_block() {
        let page = parse_html_page(
            "<ul><li>Outer<ul><li>Inner one</li><li>Inner two</li></ul></li><li>Last</li></ul>",
        );
        assert_eq!(
            page.content,
            "- Outer\n\n- Inner one\n\n- Inner two\n\n- Last"
        );
    }

    #[test]
    fn line_breaks_split_blocks() {
        let page = parse_html_page("<p>first line<br>second line<br/>third</p>");
        assert_eq!(page.content, "first line\n\nsecond line\n\nthird");
    }

    #[test]
    fn empty_and_markup_only_documents_produce_no_content() {
        for source in ["", "<html><head><script>x()</script></head><body> </body></html>"] {
            let page = parse_html_page(source);
            assert_eq!(page.content, "", "{source:?}");
            assert!(page.headings.is_empty());
            assert!(page.link_targets.is_empty());
            assert_eq!(page.author, None);
        }
    }

    #[test]
    fn corpus_scans_only_html_files_with_links_and_tier0_metadata() -> Result<()> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("membrick-html-corpus-{nanos}"));
        fs::create_dir_all(root.join("nested"))?;
        fs::write(
            root.join("guide.html"),
            "<html><head><meta name=\"author\" content=\"Ada\"></head>\
             <body><h1>Guide</h1><p>See <a href=\"nested/setup.htm\">setup</a>.</p></body></html>",
        )?;
        fs::write(root.join("nested/setup.htm"), "<h1>Setup</h1><p>Run it.</p>")?;
        fs::write(root.join("notes.md"), "# Notes\n")?;

        let result = build_html_corpus(&root, 1 << 20, 10, 10, 1 << 20);
        fs::remove_dir_all(&root)?;
        let corpus = result?;

        let mut rel_paths: Vec<_> = corpus.pages.iter().map(|p| p.rel_path.as_str()).collect();
        rel_paths.sort();
        assert_eq!(rel_paths, vec!["guide.html", "nested/setup.htm"]);

        let guide = corpus
            .pages
            .iter()
            .find(|p| p.rel_path == "guide.html")
            .expect("guide page");
        assert_eq!(guide.content, "# Guide\n\nSee setup.");
        assert!(guide.links.contains("setup"));

        let record = corpus
            .tier0_records
            .iter()
            .find(|r| r.source == "guide.html")
            .expect("guide tier0 record");
        assert_eq!(record.author_agent.as_deref(), Some("Ada"));
        assert_eq!(record.metadata.get("file_ext").map(String::as_str), Some("html"));
        Ok(())
    }
}
