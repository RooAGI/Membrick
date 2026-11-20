---
name: membrick-search-feedback
description: Submit usefulness and per-document relevance feedback for a recorded search. Include every result document exactly once; use enrich_doc for source metadata changes.
---

# Membrick search feedback

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use the search_id and full result set from review_search. Provide one judgment for every returned document, preserving the exact doc_id; the service supplies original rank and source hash. Query rule additions must be terms evidenced in the query or returned sources.
