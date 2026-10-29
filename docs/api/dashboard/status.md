# Status

Index state, query summary, integrations, and transcript captures in one
response.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/status`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{
  "status": "healthy",
  "version": "0.3.0",
  "uptime_seconds": 5400,
  "index": {
    "source_document_count": 13974,
    "record_count": 13974,
    "dirty": false,
    "store_revision": 42,
    "snapshot_revision": 42,
    "revision_lag": 0,
    "snapshot": { "layout": "segmented", "segment_count": 14, "global_document_count": 13974 }
  },
  "telemetry": {},
  "integrations": [],
  "transcript_captures": []
}
```

| Field | Description |
| --- | --- |
| `status` | `degraded` when the index has unpublished changes (`index.dirty`), otherwise `healthy` |
| `uptime_seconds` | Seconds since the server started |
| `index.revision_lag` | Gap between the store revision and the published snapshot |
| `index.snapshot` | Published snapshot layout. May be `null`, and carries more fields than shown. |
| `telemetry` | The [timeseries](timeseries.md) snapshot |
| `integrations` | Provider readiness, as in [Integrations](integrations.md) |
| `transcript_captures` | Per-session capture progress |

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `500` | `{"detail":"status unavailable"}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/status
```
