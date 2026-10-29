# Capture status

Per-session progress of queued transcript captures.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/captures`

Builds with the `claude-code` or `codex` feature only.

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{ "captures": [] }
```

Each entry reports captured bytes, target bytes, remaining bytes, the last
progress time, and the last error for one provider session. Submit captures
with [Submit a transcript capture](../provider/captures.md).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `500` | `{"detail":"..."}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/captures
```
