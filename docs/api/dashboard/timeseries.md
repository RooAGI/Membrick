# Timeseries

The query telemetry snapshot: five-second aggregates over the rolling window
plus the query summary.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/timeseries`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

The telemetry snapshot object. Its fields are described in
[Telemetry](../../telemetry.md#query-aggregates).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/timeseries
```
