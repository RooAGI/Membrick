# Refresh the search view

Publish pending index changes so they become searchable. Normal add, update,
and delete calls already publish their own changes, so most clients never
call this.

## Endpoint

- **Method:** `POST`
- **URL:** `/v1/memories/refresh`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |

## Request body

None.

## Response

### 204

Pending work has been flushed. The body is empty.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `429` | The writer is busy |
| `500` | Refresh failed |

Refresh does not create or change any memory. Use it after staged writes when
the next search must see them.

## Example

```bash
curl -X POST http://127.0.0.1:8080/v1/memories/refresh \
  -H "Authorization: Bearer $MEMBRICK_TOKEN"
```

## SDK equivalents

=== "Python"

    ```python
    memory.refresh()
    ```

=== "Node.js"

    ```js
    await memory.refresh()
    ```

=== "Rust"

    ```rust
    memory.refresh()?;
    ```
