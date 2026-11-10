use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::config::normalize_list;
use crate::graph::{Graph, Tier0Record};
use crate::source::SourceDocument;

#[derive(Debug, Clone)]
pub struct AdapterInput<'a> {
    pub root: &'a Path,
    pub max_bytes: usize,
    pub max_files: usize,
    pub max_depth: usize,
    pub max_total_bytes: usize,
}
pub trait SourceAdapter {
    fn name(&self) -> &'static str;
    fn supports(&self, path: &Path) -> bool;
    fn ingest(&self, input: &AdapterInput<'_>) -> Result<Vec<SourceDocument>>;
}

pub mod html;
pub mod image;
pub mod markdown;
pub mod txt;

/// Directory a file adapter walks for `root`, matching the Markdown scan: a
/// single file scans its parent, and a project with a `docs` directory scans
/// that directory.
pub(crate) fn scan_base(root: &Path) -> std::path::PathBuf {
    if root.is_file() {
        return root.parent().unwrap_or(root).to_path_buf();
    }
    let docs = root.join("docs");
    if docs.is_dir() {
        docs
    } else {
        root.to_path_buf()
    }
}

/// One readable file found by [`scan_text_files`].
pub(crate) struct ScannedFile {
    pub path: std::path::PathBuf,
    pub rel_path: String,
    pub extension: String,
    pub raw_concept: String,
    pub size_bytes: usize,
    pub modified: Option<std::time::SystemTime>,
    pub text: String,
}

/// Walk the scan base for `root` and read every UTF-8 file whose extension
/// `accepts`, applying the same limits as the Markdown scan. Unreadable
/// entries are skipped rather than failing the scan.
pub(crate) fn scan_text_files(
    root: &Path,
    max_bytes: usize,
    max_files: usize,
    max_depth: usize,
    max_total_bytes: usize,
    accepts: impl Fn(&Path, &str) -> bool,
) -> Vec<ScannedFile> {
    scan_files_with_text(
        root,
        max_bytes,
        max_files,
        max_depth,
        max_total_bytes,
        accepts,
        |path| Some(path.to_path_buf()),
    )
}

/// Like [`scan_text_files`], but the text of each accepted file is read from
/// the path `text_source` returns for it, such as a description file next to
/// an image. Files without a text source are skipped. Byte limits apply to
/// the text that is read, and the modification time is the later of the two
/// files.
pub(crate) fn scan_files_with_text(
    root: &Path,
    max_bytes: usize,
    max_files: usize,
    max_depth: usize,
    max_total_bytes: usize,
    accepts: impl Fn(&Path, &str) -> bool,
    text_source: impl Fn(&Path) -> Option<std::path::PathBuf>,
) -> Vec<ScannedFile> {
    let base = scan_base(root);
    let single_file = root.is_file().then(|| root.to_path_buf());
    let rel_root = if root.is_file() { base.as_path() } else { root };

    let mut files = Vec::new();
    let mut files_seen = 0usize;
    let mut total_bytes = 0usize;
    for entry in walkdir::WalkDir::new(&base).max_depth(max_depth) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if single_file.as_deref().is_some_and(|only| entry.path() != only) {
            continue;
        }
        let Some(extension) = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| accepts(entry.path(), extension))
        else {
            continue;
        };

        files_seen += 1;
        if files_seen > max_files {
            break;
        }
        let Some(text_path) = text_source(entry.path()) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&text_path) else {
            continue;
        };
        let size_bytes = metadata.len() as usize;
        if size_bytes > max_bytes {
            continue;
        }
        total_bytes = total_bytes.saturating_add(size_bytes);
        if total_bytes > max_total_bytes {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&text_path) else {
            continue;
        };
        let modified = if text_path == entry.path() {
            metadata.modified().ok()
        } else {
            let own = entry.metadata().ok().and_then(|own| own.modified().ok());
            own.max(metadata.modified().ok())
        };
        files.push(ScannedFile {
            path: entry.path().to_path_buf(),
            rel_path: crate::graph::rel_path(rel_root, entry.path()),
            extension: extension.to_string(),
            raw_concept: entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string(),
            size_bytes,
            modified,
            text,
        });
    }
    files
}

/// Tier0 record for a page built from a scanned file, with the same metadata
/// keys the Markdown scan writes.
pub(crate) fn scanned_file_tier0_record(
    file: &ScannedFile,
    page: &markdown::SourcePage,
    author_agent: Option<String>,
) -> Tier0Record {
    let mut metadata = HashMap::new();
    metadata.insert("concept".to_string(), page.concept.clone());
    metadata.insert("raw_concept".to_string(), page.raw_concept.clone());
    metadata.insert("file_ext".to_string(), file.extension.clone());
    metadata.insert("heading_count".to_string(), page.headings.len().to_string());
    metadata.insert(
        "outbound_link_count".to_string(),
        page.links.len().to_string(),
    );
    metadata.insert("path".to_string(), page.path.clone());
    metadata.insert("file_size_bytes".to_string(), file.size_bytes.to_string());
    let timestamp = file
        .modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| {
            crate::temporal::normalize_timestamp(&format!("unix:{}", duration.as_secs()))
        });
    Tier0Record {
        id: page.rel_path.clone(),
        source: page.rel_path.clone(),
        timestamp,
        author_agent,
        doc_length: file.size_bytes,
        metadata,
    }
}

/// Whether the project scan reads this file, judged by its extension. Every
/// component that decides whether a file change affects the workspace index
/// (scan, watcher, change fingerprints) uses this check so they stay in
/// agreement. Images count because adding or removing one changes whether
/// its sibling `.txt` file is indexed as a description or as plain text.
pub(crate) fn is_indexed_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension == "md"
                || html::is_html_extension(extension)
                || txt::is_txt_extension(extension)
                || image::is_image_extension(extension)
        })
}

pub fn default_source_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(markdown::MarkdownAdapter),
        Box::new(html::HtmlAdapter),
        Box::new(txt::TxtAdapter),
        Box::new(image::ImageAdapter),
    ]
}

/// Build the project graph for a corpus root.
pub fn build_project_graph(input: &AdapterInput<'_>) -> Result<Graph> {
    Graph::build(
        &input.root.to_string_lossy(),
        input.max_bytes,
        input.max_files,
        input.max_depth,
        input.max_total_bytes,
    )
}

/// Drop pages whose relative path contains any of the configured ignore
/// fragments, along with the Tier0 records that described them.
pub fn apply_ignore_paths(mut graph: Graph, ignore_paths: &[String]) -> Graph {
    if ignore_paths.is_empty() {
        return graph;
    }
    let ignore = normalize_list(ignore_paths);
    graph.pages.retain(|p| {
        let rel = p.rel_path.to_lowercase();
        !ignore.iter().any(|pat| rel.contains(pat))
    });
    let retained: HashSet<String> = graph.pages.iter().map(|p| p.rel_path.clone()).collect();
    graph.tier0_records.retain(|r| retained.contains(&r.source));
    graph
}

/// Convert a project graph into the source documents an index store ingests.
pub fn graph_to_source_documents(graph: &Graph) -> Vec<SourceDocument> {
    let mut tier0_by_source = HashMap::<String, &Tier0Record>::new();
    for record in &graph.tier0_records {
        if tier0_by_source
            .insert(record.source.clone(), record)
            .is_some()
        {
            debug_assert!(false, "duplicate Tier0 source: {}", record.source);
        }
    }
    let concept_to_rel: HashMap<String, String> = graph
        .pages
        .iter()
        .map(|p| (p.concept.clone(), p.rel_path.clone()))
        .collect();

    graph
        .pages
        .iter()
        .map(|p| {
            let t0 = tier0_by_source.get(&p.rel_path).copied();
            let mut filters = BTreeMap::new();
            if t0.and_then(|record| record.timestamp.as_ref()).is_some() {
                filters.insert("timestamp_kind".to_string(), "source_modified".to_string());
            }
            if let Some(supersedes_id) = t0.and_then(|record| record.metadata.get("supersedes_id"))
            {
                filters.insert("supersedes_id".to_string(), supersedes_id.clone());
            }
            if let Some(media_type) = t0.and_then(|record| record.metadata.get("media_type")) {
                filters.insert("media_type".to_string(), media_type.clone());
            }
            SourceDocument {
                doc_id: p.rel_path.clone(),
                source: p.rel_path.clone(),
                content: p.content.as_str().into(),
                concept: p.raw_concept.clone(),
                group_id: None,
                filters,
                headings: p.headings.clone(),
                links: p
                    .links
                    .iter()
                    .filter_map(|c| concept_to_rel.get(c).cloned())
                    .collect(),
                timestamp: t0.and_then(|r| r.timestamp.clone()),
                doc_length: t0.map(|r| r.doc_length).unwrap_or(p.content.len()),
                author_agent: t0.and_then(|r| r.author_agent.clone()),
                key_phrases: Vec::new(),
                key_phrase_extraction_hash: String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("membrick-adapters-{name}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn ignore_paths_drop_vendored_documents_and_their_tier0_records() {
        let root = temp_dir("ignore-paths");
        fs::write(root.join("readme.md"), "# Readme\nthe project itself").unwrap();
        fs::create_dir_all(root.join("node_modules").join("left-pad")).unwrap();
        fs::write(
            root.join("node_modules").join("left-pad").join("readme.md"),
            "# Left Pad\nthe project itself",
        )
        .unwrap();

        let input = AdapterInput {
            root: &root,
            max_bytes: 5_000_000,
            max_files: 50_000,
            max_depth: 20,
            max_total_bytes: 100_000_000,
        };
        let unfiltered = build_project_graph(&input).unwrap();
        assert_eq!(unfiltered.pages.len(), 2);

        let filtered = apply_ignore_paths(unfiltered, &["node_modules".to_string()]);
        let documents = graph_to_source_documents(&filtered);
        assert_eq!(documents.len(), 1);
        assert!(!documents[0].source.contains("node_modules"));
        assert!(!filtered
            .tier0_records
            .iter()
            .any(|record| record.source.contains("node_modules")));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn markdown_supersession_frontmatter_becomes_a_supersedes_filter() {
        // Search behavior for superseded guidance is covered through
        // MemoryService in memory_api::tests.
        let root = temp_dir("temporal-markdown");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("legacy.md"),
            "# Legacy ownership\nThe old team owns the control surface.\n",
        )
        .unwrap();
        fs::write(
            root.join("current.md"),
            "---\nsupersedes: legacy.md\n---\n# Current ownership\nThe new team is responsible for the control surface.\n",
        )
        .unwrap();

        let input = AdapterInput {
            root: &root,
            max_bytes: 5_000_000,
            max_files: 50_000,
            max_depth: 20,
            max_total_bytes: 50_000_000,
        };
        let graph = build_project_graph(&input).unwrap();
        let documents = graph_to_source_documents(&graph);
        let current = documents
            .iter()
            .find(|doc| doc.doc_id == "current.md")
            .expect("current Markdown document should be indexed");
        assert_eq!(
            current.filters.get("supersedes_id").map(String::as_str),
            Some("legacy.md")
        );
        let legacy = documents
            .iter()
            .find(|doc| doc.doc_id == "legacy.md")
            .expect("legacy Markdown document should be indexed");
        assert!(!legacy.filters.contains_key("supersedes_id"));

        let _ = fs::remove_dir_all(&root);
    }

    fn input_for(root: &Path, max_files: usize, max_total_bytes: usize) -> AdapterInput<'_> {
        AdapterInput {
            root,
            max_bytes: 5_000_000,
            max_files,
            max_depth: 20,
            max_total_bytes,
        }
    }

    fn sorted_page_paths(graph: &Graph) -> Vec<String> {
        let mut paths: Vec<_> = graph.pages.iter().map(|p| p.rel_path.clone()).collect();
        paths.sort();
        paths
    }

    #[test]
    fn project_graph_merges_every_source_format() {
        let root = temp_dir("all-formats");
        fs::write(root.join("guide.md"), "# Guide\nMarkdown body.").unwrap();
        fs::write(root.join("page.html"), "<h1>Page</h1><p>HTML body.</p>").unwrap();
        fs::write(root.join("notes.txt"), "Plain notes.").unwrap();
        fs::write(root.join("photo.jpg"), [0xFF, 0xD8]).unwrap();
        fs::write(root.join("photo.txt"), "A harbor at dusk.").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let graph = build_project_graph(&input_for(&root, 50_000, 100_000_000)).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            sorted_page_paths(&graph),
            vec!["guide.md", "notes.txt", "page.html", "photo.jpg"]
        );
        assert_eq!(graph.tier0_records.len(), 4);

        let documents = graph_to_source_documents(&graph);
        let photo = documents.iter().find(|d| d.doc_id == "photo.jpg").unwrap();
        assert_eq!(
            photo.filters.get("media_type").map(String::as_str),
            Some("image/jpeg")
        );
        assert!(photo.content.contains("A harbor at dusk."));
        let guide = documents.iter().find(|d| d.doc_id == "guide.md").unwrap();
        assert!(!guide.filters.contains_key("media_type"));
    }

    #[test]
    fn project_graph_shares_the_file_limit_across_formats() {
        let root = temp_dir("file-budget");
        fs::write(root.join("a.md"), "# A").unwrap();
        fs::write(root.join("b.md"), "# B").unwrap();
        fs::write(root.join("c.html"), "<h1>C</h1>").unwrap();
        fs::write(root.join("d.txt"), "D").unwrap();

        let limited = build_project_graph(&input_for(&root, 3, 100_000_000)).unwrap();
        let unlimited = build_project_graph(&input_for(&root, 50_000, 100_000_000)).unwrap();
        let _ = fs::remove_dir_all(&root);

        // Markdown is scanned first, then HTML gets the one remaining slot.
        assert_eq!(sorted_page_paths(&limited), vec!["a.md", "b.md", "c.html"]);
        assert_eq!(unlimited.pages.len(), 4);
    }

    #[test]
    fn project_graph_shares_the_byte_limit_across_formats() {
        let root = temp_dir("byte-budget");
        let markdown = format!("# Notes\n{}", "m".repeat(52)); // 60 bytes
        let html = format!("<p>{}</p>", "h".repeat(53)); // 60 bytes
        fs::write(root.join("notes.md"), &markdown).unwrap();
        fs::write(root.join("page.html"), &html).unwrap();

        let limited = build_project_graph(&input_for(&root, 50_000, 100)).unwrap();
        let enough = build_project_graph(&input_for(&root, 50_000, 120)).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(sorted_page_paths(&limited), vec!["notes.md"]);
        assert_eq!(sorted_page_paths(&enough), vec!["notes.md", "page.html"]);
    }

    #[test]
    fn indexed_source_paths_follow_each_adapter_extension_rule() {
        for indexed in [
            "a.md",
            "docs/b.html",
            "c.htm",
            "d.txt",
            "e.jpg",
            "f.jpeg",
            "g.png",
            "IMG_001.JPG",
            "h.Png",
        ] {
            assert!(is_indexed_source_path(Path::new(indexed)), "{indexed}");
        }
        // Markdown and text matching stay case sensitive, as before.
        for skipped in ["a.MD", "b.HTML", "c.TXT", "d.gif", "e.rs", "f", ".md", "g.md.bak"] {
            assert!(!is_indexed_source_path(Path::new(skipped)), "{skipped}");
        }
    }

    fn set_modified(path: &Path, seconds: u64) {
        let time = UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_described_file_takes_the_later_of_its_two_modified_times() {
        let root = temp_dir("modified");
        for name in ["older-image.png", "older-text.png"] {
            fs::write(root.join(name), [0x89]).unwrap();
        }
        fs::write(root.join("older-image.txt"), "x").unwrap();
        fs::write(root.join("older-text.txt"), "x").unwrap();
        set_modified(&root.join("older-image.png"), 1_000_000);
        set_modified(&root.join("older-image.txt"), 2_000_000);
        set_modified(&root.join("older-text.png"), 3_000_000);
        set_modified(&root.join("older-text.txt"), 1_000_000);

        let files = scan_files_with_text(
            &root,
            1 << 20,
            10,
            10,
            1 << 20,
            |_, extension| extension == "png",
            |path| Some(path.with_extension("txt")),
        );
        let _ = fs::remove_dir_all(&root);

        let modified = |name: &str| {
            files
                .iter()
                .find(|file| file.rel_path == name)
                .and_then(|file| file.modified)
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
        };
        assert_eq!(modified("older-image.png"), Some(2_000_000));
        assert_eq!(modified("older-text.png"), Some(3_000_000));
    }

    #[test]
    fn a_single_file_root_scans_only_that_file_for_each_format() {
        let root = temp_dir("single-file");
        fs::write(root.join("one.html"), "<h1>One</h1>").unwrap();
        fs::write(root.join("two.html"), "<h1>Two</h1>").unwrap();
        fs::write(root.join("one.txt"), "One").unwrap();
        fs::write(root.join("two.txt"), "Two").unwrap();
        fs::write(root.join("lone.png"), [0x89]).unwrap();
        fs::write(root.join("one-shot.png"), [0x89]).unwrap();
        fs::write(root.join("one-shot.txt"), "Described").unwrap();

        let graph_for = |file: &str| {
            build_project_graph(&input_for(&root.join(file), 50_000, 100_000_000)).unwrap()
        };
        let html = graph_for("one.html");
        let text = graph_for("one.txt");
        let image = graph_for("one-shot.png");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(sorted_page_paths(&html), vec!["one.html"]);
        assert_eq!(sorted_page_paths(&text), vec!["one.txt"]);
        assert_eq!(sorted_page_paths(&image), vec!["one-shot.png"]);
    }
}
