# Search reviews

A page of rows from the latest offline search-feedback snapshot.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/search-reviews`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Query parameters

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `offset` | integer | No | `0` | Rows to skip |
| `page_size` | integer | No | `25` | Rows per page |

## Response

### 200

A page-ready snapshot of review rows. See
[Telemetry](../../telemetry.md#dashboard-and-api).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `500` | `{"detail":"search reviews unavailable: ..."}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  "http://127.0.0.1:8080/api/search-reviews?offset=0&page_size=25"
```
