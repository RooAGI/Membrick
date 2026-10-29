# Tier0 scan metadata

Tier0 is the lightweight metadata collected while Membrick scans Markdown
files. It is a helper stage for project graph and export workflows. The
resident's searchable store uses `SourceDocument` and derived `DocRecord`
values; `Tier0Record` is not the persistent memory API or the full search
record.

## Current record

The internal `Tier0Record` in `src/graph.rs` contains:

| Field | Meaning |
| --- | --- |
| `id` | Stable scan ID, currently the path relative to the scan root |
| `source` | Relative source path |
| `timestamp` | Source modified time, normalized to a Unix-seconds timestamp when available |
| `author_agent` | Author or agent found in supported frontmatter keys |
| `doc_length` | Source content length in bytes |
| `metadata` | Lightweight derived values such as concept, extension, heading and link counts, file size, and frontmatter flags |

Records are joined to parsed Markdown pages by their relative source path.
Ignored pages are removed together with their Tier0 records. The current
filesystem adapter scans Markdown; other file types are not indexed through
this adapter today.

## Build the searchable project index

Use the project command to build or refresh the persistent search index:

```bash
membrick index /path/to/project
```

The search store is maintained under `.membrick`. For applications that
submit or update documents directly, use the [Memory client](memory-client-architecture.md)
or the Rust [`MemoryService`](memory-service-api.md) API rather than treating
Tier0 export as the index format.

## Legacy export

The hidden `--tier0-index-out` option remains for older scripts. It writes a
standalone JSON summary with fields such as `tier`, `generated_at_unix`,
`document_count`, and `documents_by_id`. This compatibility export is not the
resident store format and is not required for normal indexing or search.
