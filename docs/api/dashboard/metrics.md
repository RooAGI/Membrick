# Metrics (JSON)

Query summary and per-provider counters. The Prometheus form of the same data
is [`GET /metrics`](../operations/metrics.md).

## Endpoint

- **Method:** `GET`
- **URL:** `/api/metrics`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{
  "query": { "requests": 128, "errors": 0, "empty_results": 3 },
  "providers": [
    {
      "provider": "claude",
      "compiled": true,
      "state": "active",
      "events_total": 310,
      "sessions_started": 4,
      "sessions_ended": 3,
      "sessions_active": 1,
      "retrieval_events": 96,
      "capture_events": 88
    }
  ]
}
```

`query` is the query summary: counts, rates, and latency quantiles.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/metrics
```
