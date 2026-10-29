# Provider memory

Agent integrations such as Claude Code and Codex share one captured-memory
store per project. These routes are how their hooks write to it and search it.
Most applications should use the [memory routes](../index.md) instead.

The resident server owns the workspace and captured-memory stores. Providers
never hold writable indexes.

| Route | Page |
| --- | --- |
| `POST /provider-memory/add/batch` | [Add a batch](add-batch.md) |
| `POST /provider-memory/search` | [Search](search.md) |
| `POST /provider-memory/captures` | [Submit a transcript capture](captures.md) |

These routes exist only in builds that include an agent integration feature
(`claude-code`, `codex`, `gemini-cli`, `agy`, `muse-code`, `openclaw`,
`hermes`, or `roo-runtime`). The captures route needs `claude-code` or `codex`.

Two further routes carry the MCP adapters' traffic to the resident server:
`POST /provider-memory/mcp/tools/call` and
`POST /provider-memory/mcp/documents/upsert`. They are internal, accept only the
server token, and are not a stable public API. `POST
/integrations/openclaw/hooks/{kind}` receives OpenClaw hook events in builds
with the `openclaw` feature and also needs the server token.
