# Submit a transcript capture

Queue a provider transcript for background capture. The resident drains it
independently of the hook process.

## Endpoint

- **Method:** `POST`
- **URL:** `/provider-memory/captures`
- **Content-Type:** `application/json`

Builds with the `claude-code` or `codex` feature only.

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |
| `Content-Type` | Yes | `application/json` |

The route is unavailable in tenant mode and returns `403`.

## Request body

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `provider` | string | Yes | `claude` or `codex` |
| `document_type` | string | Yes | The kind of transcript document being captured |
| `input` | object | Yes | Provider payload. It must include a non-empty `session_id`. |
| `workspace` | string | No | Project root. Defaults to the primary workspace. |

## Response

### 202

```json
{ "accepted": true, "capture": {} }
```

Accepted means queued. It does not mean captured or searchable. Follow progress
with [Capture status](../dashboard/captures.md).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used, or tenant mode is on |
| `422` | The body or payload failed validation |
| `503` | The capture worker could not take the job |

Pending requests are saved under `.membrick/runtime/transcript-captures/`. A
failed write keeps the cursor in place and retries with backoff.
