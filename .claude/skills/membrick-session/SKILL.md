---
name: membrick-session
description: Bind an MCP connection to a specific Codex session. Installed Codex lifecycle hooks call this automatically; use only when explicit session binding is needed.
---

# Membrick membrick session

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
This is a Codex lifecycle binding operation. Installed hooks call it automatically; do not bind arbitrary sessions or call it as a substitute for search.
