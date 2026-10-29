# Get a memory

Retrieve one memory by its opaque ID. The user must own it.

## Endpoint

- **Method:** `GET`
- **URL:** `/v1/memories/{memory_id}`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |

## Path parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `memory_id` | string | Yes | ID returned by [add](add.md), [search](search.md), or [list](list.md) |

## Query parameters

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `user_id` | string | Yes | none | Owner ID |
| `include_inactive` | boolean | No | `false` | Also return expired or superseded memories |

## Response

### 200

```json
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
```

`created_at` is RFC 3339. `expires_at_ms` is Unix milliseconds. Optional values
are `null` when absent.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `404` | The memory does not exist, belongs to another user, or is inactive and `include_inactive` was not set |

## Example

```bash
curl "http://127.0.0.1:8080/v1/memories/$MEMORY_ID?user_id=user-123" \
  -H "Authorization: Bearer $MEMBRICK_TOKEN"
```

## SDK equivalents

=== "Python"

    ```python
    record = memory.get("<memory-id>", "user-123")  # None if not found
    ```

=== "Node.js"

    ```js
    const record = await memory.get('<memory-id>', 'user-123')
    ```

=== "Rust"

    ```rust
    use membrick::GetRequest;

    let record = memory.get(GetRequest {
        user_id: "user-123".into(),
        memory_id: "<memory-id>".into(),
        include_inactive: false,
    })?; // Option<MemoryRecord>
    ```
