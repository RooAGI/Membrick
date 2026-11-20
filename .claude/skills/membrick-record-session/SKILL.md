---
name: membrick-record-session
description: Start, stop, or inspect opt-in local session recording. Recording captures redacted lifecycle events and does not inject memory; do not start it implicitly.
---

# Membrick record session

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use action `start`, `stop`, or `status` only when the user requests session recording control. Recording is local, opt-in, and capture-only.
