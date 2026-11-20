---
name: membrick-enrich-doc
description: Add evidence-backed searchable metadata to a source document. Use exact doc_id and expected_source_hash values from search or review_search, and cite evidence.
---

# Membrick enrich doc

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use exact `doc_id` and `expected_source_hash` from a returned source. Include evidence references with exact source hashes. Add only supported evidence-based metadata; use search_feedback for evaluation labels.
