# HTTP API overview

The Membrick server exposes a JSON-over-HTTP API. This page covers what every
route shares: the base URL, authentication, errors, and limits. Each route
group has its own reference pages.

## Base URL

The server binds to `127.0.0.1:8080` by default.

```bash
cargo run --release --bin server -- --settings .membrick/server.toml
curl http://127.0.0.1:8080/health
```

See [HTTP server setup](../server.md) for startup options, and
[Docker](../docker.md) for running the server in a container. Binding to a
non-loopback address requires `--allow-non-loopback` and a configured
`server_token` or `jwt_secret`.

## Route groups

| Group | Routes | Auth |
| --- | --- | --- |
| [Memories](index.md) | `/v1/memories`, `/v1/memories/{memory_id}`, `/v1/memories/search`, `/v1/memories/refresh` | Token or JWT |
| [Boards](boards/index.md) | `/api/boards`, `/api/boards/{board_id}` | Server token |
| [Flows](flows/index.md) | `/api/flows`, `/api/flows/{flow_id}` | Server token |
| [Provider memory](provider/index.md) | `/provider-memory/add/batch`, `/provider-memory/search`, `/provider-memory/captures`; internal adapter routes `/provider-memory/mcp/tools/call`, `/provider-memory/mcp/documents/upsert`; OpenClaw hooks `/integrations/openclaw/hooks/{kind}` | Token or JWT; adapter routes and hooks need the server token |
| [Operations](operations/index.md) | `/health`, `/metrics` | None for `/health`; token for `/metrics` |
| [Dashboard telemetry](dashboard/index.md) | `/api/status`, `/api/timeseries`, `/api/events`, `/api/sessions`, `/api/integrations`, `/api/search-reviews`, `/api/metrics`, `/api/captures` | Server token only |

The provider routes exist only in builds that include an agent integration
feature. `/provider-memory/captures` and `/api/captures` exist only in builds
with the `claude-code` or `codex` feature. The MCP adapter bridge routes are
internal transport endpoints, not a stable application API. OpenClaw hooks
exist only in builds with the `openclaw` feature. Board/Post and FlowRun routes
exist only when the Flowboard extensions are linked into the host.

## Authentication

Send JSON with `Content-Type: application/json`. The server supports three
modes, set in the server settings file.

| Mode | Setting | How to authenticate |
| --- | --- | --- |
| None | neither `server_token` nor `jwt_secret` | No header. Loopback only. |
| Shared token | `server_token` or `server_token_file` | `Authorization: Bearer <token>` (or `Token <token>`), or `x-api-key: <token>` |
| JWT | `jwt_secret` | `Authorization: Bearer <jwt>` |

A JWT must be signed with HS256 and carry a non-empty `sub` and a valid `exp`.
The `sub` is the authenticated `user_id`. Memory routes check any `user_id` in
the request against it. When both a token and a JWT secret are configured, a
valid server token is accepted on workspace routes and a JWT everywhere else.

A JWT identifies one user. It cannot call workspace routes, which read
telemetry or write across the whole project: every `/api/*` route, `/metrics`,
`/provider-memory/captures`, and the OpenClaw hooks. Those return `403` with a
JWT and need the shared server token.

With authentication enabled, the server also rejects requests whose browser
`Origin` or `Host` header is not allowed, with `403`.

## Single-tenant mode

Set `tenant_id` (or `--tenant-id`) to pin the server to one tenant. A request
whose `user_id` does not match is rejected, so a token cannot select another
tenant by changing the body. Workspace MCP operations are unavailable in this
mode.

## Errors

Most handler errors use a JSON body with a `detail` string:

```json
{ "detail": "unauthorized" }
```

| Status | Meaning |
| --- | --- |
| `400` | Malformed JSON or an invalid query combination, such as `latest` with `after_sequence` |
| `401` | Missing or invalid credentials |
| `403` | Credentials are valid but not allowed: a JWT on a workspace route, a tenant mismatch, or a disallowed browser origin |
| `404` | The memory does not exist, or belongs to another user |
| `408` | The request exceeded the 30 second timeout |
| `409` | A `request_id` is reused with different content or session; see [add memories](add.md) |
| `413` | The request body exceeds the 16 MiB limit |
| `415` | A JSON route received a missing or unsupported content type |
| `422` | The body failed validation |
| `429` | The writer queue is full. Retry later with the same `request_id`. |
| `500` | Internal error |
| `503` | The requested subsystem is unavailable, such as the staged writer |

Extractor errors such as malformed JSON, unsupported content types, or oversized
bodies may use plain-text responses instead of the `detail` JSON shape. Some
`422` and `503` responses carry no body.

## Limits

- Request bodies are limited to 16 MiB.
- Requests time out after 30 seconds.
- Primary adds go through a bounded queue and a single writer. Reads continue
  against the last published view while a write is processed.

## Writes and visibility

A write can be accepted before it is searchable. `POST /v1/memories` accepts
`?wait_for_visibility=true` to return only once the new view is published. You
can also call [refresh](refresh.md). The server generates `request_id` when it
is omitted. For retries that must survive a lost response or process restart,
provide a stable ID and reuse it only for the same request. See
[Identity and IDs](index.md#identity-and-ids).
