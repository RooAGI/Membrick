# Provider add batch

Write 1 to 128 captured memories to the shared project store.

## Endpoint

- **Method:** `POST`
- **URL:** `/provider-memory/add/batch`
- **Content-Type:** `application/json`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](../http-overview.md#authentication). |
| `Content-Type` | Yes | `application/json` |

## Request body

A JSON array of 1 to 128 objects, each with the fields of
[Add memories](../add.md#request-body). There is no workspace field. The route
always writes to the primary workspace.

## Response

### 200

The add response for the batch.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | Tenant or ownership check failed |
| `422` | Empty batch, more than 128 requests, or an invalid request |
| `429` | `{"detail":"writer busy"}` |
| `503` | No staged writer is available |

## Example

```bash
curl -X POST http://127.0.0.1:8080/provider-memory/add/batch \
  -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '[{"request_id":"r1","user_id":"user-123","session_id":"s1","messages":[{"role":"user","content":"Decision: use the segmented index."}]}]'
```
