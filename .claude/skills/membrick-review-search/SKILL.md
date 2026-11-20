---
name: membrick-review-search
description: Inspect recorded MCP searches by search_id or page through unreviewed searches. Reading does not mark searches reviewed; retry if the response says indexing.
---

# Membrick review search

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Pass `search_id` for one search record. Omit it to page through unreviewed searches using the returned cursor and page_size. An indexing status means retry after the index rebuilds.
