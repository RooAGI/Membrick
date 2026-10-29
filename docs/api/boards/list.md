# List boards

Every board in the workspace.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/boards`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{
  "boards": [
    {
      "board_id": "board:mcp:%2Fwork%2Fproject:planning",
      "owner": "mcp",
      "workspace": "/work/project",
      "key": "planning",
      "title": "Planning",
      "type": "general",
      "links": [],
      "created_at": "2026-10-20T16:00:00Z"
    }
  ]
}
```

Boards created from a template also carry `board_template` and `fields`.
`type` is descriptive only and does not change behavior.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `500` | `{"detail":"boards unavailable: ..."}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/boards
```
