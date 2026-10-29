# Prometheus metrics

Aggregate query and agent activity metrics in Prometheus text format.

## Endpoint

- **Method:** `GET`
- **URL:** `/metrics`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

`Content-Type` is Prometheus text exposition.

```text
# HELP membrick_query_requests_window Search requests in the retained telemetry window.
# TYPE membrick_query_requests_window gauge
membrick_query_requests_window 128
```

Metrics are aggregates over the retained telemetry window and include no query
text. The metric list and a scrape configuration are in
[Prometheus](../../prometheus.md). The same data as JSON is in
[Metrics (JSON)](../dashboard/metrics.md).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/metrics
```
