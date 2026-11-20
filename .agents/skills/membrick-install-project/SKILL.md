---
name: membrick-install-project
description: Install project-scoped MCP, hooks, and guidance for an explicit project path and provider. This changes provider configuration; only use when the user asks to install or enable Membrick.
---

# Membrick membrick install project

Use the corresponding Membrick MCP tool when the user’s request matches this capability. Follow the tool’s live schema and response; do not guess missing identifiers or report success when the tool returns an error.
Use explicit project_path and provider. This writes provider configuration. For AGY, installation is global and can replace its manager configuration across folders; surface that consequence before proceeding.
