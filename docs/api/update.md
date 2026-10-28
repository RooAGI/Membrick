# Update a memory

Replace the text and selected metadata of one active memory, keeping its ID.
When the content changes, Membrick refreshes the derived search phrases
before returning.

## Endpoint

- **Method:** `PATCH`
- **URL:** `/v1/memories/{memory_id}`
- **Content-Type:** `application/json`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |
| `Content-Type` | Yes | `application/json` |

## Path parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `memory_id` | string | Yes | ID of an active memory. The path is authoritative. |

## Request body

```json
{
  "user_id": "user-123",
  "content": "I prefer unsweetened oat milk.",
  "role": "user"
}
```

| Field | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `user_id` | string | Yes | none | Owner ID |
| `content` | string | Yes | none | Replacement text. Non-empty and within the message size limit. |
| `role` | string | No | existing role, else `user` | `user` or `assistant` |
| `timestamp` | integer or null | No | none | Source timestamp in Unix milliseconds. Omitting it clears the existing one. |
| `expires_at_ms` | integer or null | No | none | Expiration in Unix milliseconds. Omitting or `null` clears it. |

## Response

### 200

The updated record, in the same shape as [Get a memory](get.md#200).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `404` | The memory does not exist, is inactive, or belongs to another user |
| `422` | Invalid request |

Update keeps the memory ID. To keep correction history, [add](add.md) a new
memory with `supersedes_id` instead.

## Example

```bash
curl -X PATCH "http://127.0.0.1:8080/v1/memories/$MEMORY_ID" \
  -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user-123", "content": "I prefer unsweetened oat milk."}'
```

## SDK equivalents

=== "Python"

    ```python
    record = memory.update("<memory-id>", "user-123", "I prefer unsweetened oat milk.", role="user")
    ```

=== "Node.js"

    ```js
    const record = await memory.update(
      '<memory-id>',
      'user-123',
      'I prefer unsweetened oat milk.',
      'user',
    )
    ```

=== "Rust"

    ```rust
    use membrick::UpdateRequest;

    let record = memory.update(UpdateRequest {
        user_id: "user-123".into(),
        memory_id: "<memory-id>".into(),
        content: "I prefer unsweetened oat milk.".into(),
        role: Some("user".into()),
        timestamp: None,
        expires_at_ms: None,
    })?; // Option<MemoryRecord>
    ```
