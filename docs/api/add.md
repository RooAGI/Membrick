# Add memories

Store one request containing one or more user or assistant messages. Each
message becomes an independently retrievable memory.

## Endpoint

- **Method:** `POST`
- **URL:** `/v1/memories`
- **Content-Type:** `application/json`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |
| `Content-Type` | Yes | `application/json` |

## Query parameters

| Parameter | Type | Default | Description |
| --- | --- | --- | --- |
| `wait_for_visibility` | boolean | `false` | Return only after the write is searchable |

## Request body

```json
{
  "request_id": "req-001",
  "user_id": "user-123",
  "session_id": "chat-456",
  "messages": [
    { "role": "user", "content": "I prefer oat milk." },
    { "role": "assistant", "content": "I will remember that preference." }
  ]
}
```

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `request_id` | string | No | Server-generated when omitted; caller-supplied IDs make retries durable |
| `user_id` | string | Yes | Memory owner |
| `session_id` | string | No | Conversation grouping and follow-up context key; generated when omitted |
| `messages` | array | Yes | 1 to 1,024 messages |
| `messages[].role` | string | Yes | `user` or `assistant` |
| `messages[].content` | string | Yes | Non-empty text, up to 1 MiB |
| `messages[].timestamp` | integer or null | No | Unix time in milliseconds |
| `messages[].expires_at_ms` | integer or null | No | Expiration time in Unix milliseconds |
| `messages[].supersedes_id` | string or null | No | ID of an existing memory this message corrects |

Supplied IDs must be non-empty, at most 256 UTF-8 bytes, and contain no
control characters.

## Response

### 200

```json
{
  "success": true,
  "request_id": "req-001",
  "user_id": "user-123",
  "session_id": "chat-456",
  "adjudication": {
    "request_id": "req-001",
    "doc_ids": ["<opaque-memory-id>", "<opaque-memory-id>"],
    "claims": [],
    "decisions": [],
    "retired_doc_ids": []
  }
}
```

`doc_ids` are the memory IDs to use with [get](get.md), [update](update.md),
and [delete](delete.md). `adjudication` reports the semantic claims and
relationship decisions made for the write. It may be absent in responses
from older versions.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | Tenant or ownership check failed |
| `409` | `request_id` reused with different content or session |
| `422` | Invalid request |
| `429` | The writer queue is full. Retry with the same `request_id`. |
| `503` | No writable service is available |

**Compatibility note:** Earlier server versions returned `422` when a
`request_id` was reused with a different payload. The current server returns
`409 Conflict` for that case. REST callers that branch on the status code
should handle `409`. Rust, Python, and Node SDKs classify both responses as a
rejected 4xx error.

## Idempotency

The first successful request stores the messages and ties their content
fingerprint to `(user_id, request_id)`. Retrying the exact same request is safe
and creates no duplicates. When `request_id` is explicit but `session_id` is
omitted, the server resolves a stable session from the owner and request ID.
Reusing the ID with different messages or a different explicit session is
rejected with `409`. After a timeout, retry with the same explicit
`request_id` and payload instead of a new ID. If the caller omitted the ID and
lost the response, it cannot identify that write for an application-level
retry; provide a durable ID when that recovery matters.

For an upgrade-spanning retry, the server can recover the original session
when the earlier write is still present in the same store and its request ID,
messages, and metadata match exactly. It returns that original session and
does not add another memory. A changed payload or any explicitly supplied
different session (including the new canonical derived ID when it differs
from the old stored session) remains a conflict. Supplying the original
stored session still deduplicates normally. If the earlier write is no longer
present, this recovery path cannot identify it.

## Example

```bash
curl -X POST "http://127.0.0.1:8080/v1/memories?wait_for_visibility=true" \
  -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "request_id": "req-001",
    "user_id": "user-123",
    "session_id": "chat-456",
    "messages": [{ "role": "user", "content": "I prefer oat milk." }]
  }'
```

## SDK equivalents

=== "Python"

    ```python
    response = memory.add(
        [{"role": "user", "content": "I prefer oat milk."}],
        user_id="user-123",
        request_id="req-001",
        session_id="chat-456",
    )
    ```

=== "Node.js"

    ```js
    const response = await memory.add(
      [{ role: 'user', content: 'I prefer oat milk.' }],
      'user-123',
      { requestId: 'req-001', sessionId: 'chat-456' },
    )
    ```

=== "Rust"

    ```rust
    use membrick::{AddOptions, Memory, Message};

    let memory = Memory::new(".")?;
    let response = memory.add(
        vec![Message {
            role: "user".into(),
            content: "I prefer oat milk.".into(),
            timestamp: None,
            expires_at_ms: None,
            supersedes_id: None,
        }],
        "user-123",
        AddOptions {
            request_id: Some("req-001".into()),
            session_id: Some("chat-456".into()),
            ..AddOptions::default()
        },
    )?;
    ```

Rust, Python, and Node.js clients also expose batch-add methods. The REST API
accepts one add request per call; see the [language guides](../index.md#related-references)
for SDK details.
