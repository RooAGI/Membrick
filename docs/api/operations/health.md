# Health check

Check that the server is running. It needs no authentication, so it works as a
liveness probe.

## Endpoint

- **Method:** `GET` (`POST` is also accepted)
- **URL:** `/health`

## Headers

None.

## Response

### 200

```json
{
  "status": "ok",
  "version": "0.3.0",
  "workspace_id": "9f2c…"
}
```

| Field | Description |
| --- | --- |
| `status` | `ok` while the server is running |
| `version` | The server version |
| `workspace_id` | SHA-256 hex digest of the canonical project root. Clients use it to confirm they reached the server for the workspace they expect. |

For index health, use [Status](../dashboard/status.md), which reports `healthy`
or `degraded`.

## Example

```bash
curl http://127.0.0.1:8080/health
```
