# List memories

Page through a user's memories in deterministic ID order using an opaque
cursor.

## Endpoint

- **Method:** `GET`
- **URL:** `/v1/memories`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |

## Query parameters

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `user_id` | string | Yes | none | Owner ID |
| `session_id` | string | No | none | Restrict the list to one conversation session |
| `limit` | integer | No | 100 | Page size, clamped to 1 to 100 |
| `cursor` | string | No | none | `next_cursor` from the previous response |
| `include_inactive` | boolean | No | `false` | Also return expired or superseded memories |

## Response

### 200

```json
{
  "data": [
    {
      "id": "<opaque-memory-id>",
      "content": "user: I prefer oat milk.",
      "role": "user",
      "user_id": "user-123",
      "session_id": "chat-456",
      "created_at": "2026-10-26T19:00:00Z",
      "expires_at_ms": null,
      "supersedes_id": null,
      "metadata": {}
    }
  ],
  "next_cursor": null
}
```

Keep requesting while `next_cursor` is not `null`, passing it back unchanged.
The cursor is the last returned memory ID. It is not an offset.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `422` | Invalid query parameters |

## Example

```bash
curl "http://127.0.0.1:8080/v1/memories?user_id=user-123&limit=25" \
  -H "Authorization: Bearer $MEMBRICK_TOKEN"
```

## SDK equivalents

=== "Python"

    ```python
    page = memory.list("user-123", limit=25)
    if page["next_cursor"]:
        page = memory.list("user-123", limit=25, cursor=page["next_cursor"])
    ```

=== "Node.js"

    ```js
    const page = await memory.list('user-123', undefined, 25)
    if (page.next_cursor) {
      const next = await memory.list('user-123', undefined, 25, page.next_cursor)
    }
    ```

=== "Rust"

    ```rust
    use membrick::ListRequest;

    let page = memory.list(ListRequest {
        user_id: "user-123".into(),
        session_id: None,
        limit: 25,
        cursor: None,
        include_inactive: false,
    })?;
    ```
