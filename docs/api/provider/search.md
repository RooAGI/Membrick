# Provider search

Search the shared captured-memory store together with the workspace-memory
index.

## Endpoint

- **Method:** `POST`
- **URL:** `/provider-memory/search`
- **Content-Type:** `application/json`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](../http-overview.md#authentication). |
| `Content-Type` | Yes | `application/json` |

## Request body

The fields of [Search memories](../search.md#request-body), plus:

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `workspace` | string | No | Project root to search. Defaults to the server's primary workspace. |

The conversation-state scope comes from the caller. With JWT authentication it
is the token's user. In tenant mode it is the tenant.

## Response

### 200

The search response, `{ "data": [...] }`, in the shape of
[Search memories](../search.md#200).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | Tenant or ownership check failed |
| `422` | `{"detail":"invalid request"}` |

## Example

```bash
curl -X POST http://127.0.0.1:8080/provider-memory/search \
  -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query":"why did we pick the segmented index","user_id":"user-123","top_k":5}'
```
