---
name: membrick-add-memory
description: Store a durable fact, decision, or observation, or correct an earlier memory. Supply a unique request_id and use supersedes_id when replacing a known memory.
---

# Membrick add memory

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use a unique `request_id` for each logical write and reuse it unchanged on retry. To correct a memory, set `supersedes_id` to the old memory ID; do not silently rewrite history.
