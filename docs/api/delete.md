# Delete a memory

Permanently remove one memory from the active corpus.

## Endpoint

- **Method:** `DELETE`
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

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `user_id` | string | Yes | Owner ID |

## Response

### 204

The memory was removed. The body is empty.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `404` | The memory does not exist or belongs to another user |
| `429` | The writer is busy |

Delete is permanent. If the content was wrong but history is useful,
[add](add.md) a corrected memory with `supersedes_id` instead.

## Example

```bash
curl -X DELETE "http://127.0.0.1:8080/v1/memories/$MEMORY_ID?user_id=user-123" \
  -H "Authorization: Bearer $MEMBRICK_TOKEN"
```

## SDK equivalents

=== "Python"

    ```python
    deleted = memory.delete("user-123", "<memory-id>")  # True if removed
    ```

=== "Node.js"

    ```js
    const deleted = await memory.delete('user-123', '<memory-id>')
    ```

=== "Rust"

    ```rust
    let deleted = memory.delete("user-123", "<memory-id>")?; // bool
    ```
