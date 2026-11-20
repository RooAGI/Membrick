---
name: membrick-search
description: Search the indexed workspace for relevant memories. Use for project history, prior decisions, earlier work, and other indexed context; pass a concise query and optionally provider, session_id, or top_k. Treat results as retrieved context and verify current-state claims against source when needed.
---

# Membrick search

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use `query` as a specific natural-language question. Omit `provider` to search the shared pool; add it only when narrowing to a provider’s memories. Use returned memory IDs when following up with get_memory. Search results can be stale, so confirm current implementation in source.
