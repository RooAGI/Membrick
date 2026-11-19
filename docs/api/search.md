# Search memories

Rank a user's memories against a query. Hits carry the memory ID, content,
score, and available evidence metadata. The response includes a `search_id` for
reviewing the exact query and ranked result snapshot later.

## Endpoint

- **Method:** `POST`
- **URL:** `/v1/memories/search`
- **Content-Type:** `application/json`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | `Bearer <token>`. See [authentication](http-overview.md#authentication). |
| `Content-Type` | Yes | `application/json` |

## Request body

```json
{
  "query": "What milk do I prefer?",
  "user_id": "user-123",
  "top_k": 5,
  "session_id": "chat-456",
  "filters": { "provider": "claude-code" }
}
```

| Field | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `query` | string | Yes | none | Search question or terms |
| `user_id` | string | Yes | none | Owner scope for memories |
| `top_k` | integer | No | 10 | Maximum number of hits |
| `session_id` | string | No | none | Enables bounded follow-up query resolution. It does not restrict results to that session. |
| `filters` | object of strings | No | none | Document metadata filters |
| `scope` | string | No | none | Conversation-state scope, separate from document ownership |
| `reference_date` | string | No | today | Date anchor for relative time expressions |
| `options` | array of strings | No | none | Reserved; accepted by the schema and SDKs but currently has no effect. |

With `session_id`, Membrick uses recent conversation state to interpret
follow-up phrasing and time anchors. To list only one session's memories, use
[List memories](list.md) with `session_id`.

## Response

### 200

```json
{
  "search_id": "memory:mem-123-456-0",
  "data": [
    {
      "id": "<opaque-memory-id>",
      "content": "user: I prefer oat milk.",
      "score": 0.82,
      "created_at": "2026-10-26T19:00:00Z",
      "role": "user",
      "user_id": "user-123",
      "session_id": "chat-456"
    }
  ]
}
```

| Field | Description |
| --- | --- |
| `search_id` | ID for reviewing this exact search and its result snapshot |
| `data` | Ranked result array |

Each item in `data` has these fields:

| Field | Description |
| --- | --- |
| `id` | Opaque memory ID |
| `content` | Stored memory text |
| `score` | Relative retrieval score |
| `created_at` | RFC 3339 creation time, when available |
| `role`, `user_id`, `session_id` | Source metadata, when available |
| `relation_evidence` | Evidence for a structured relation hit, when present |
| `comparison_operands` | Query comparison sides the hit was retrieved for, when present |
| `claim_states` | Current or superseded claim details, when present |

Empty optional fields are omitted. Ordering is the relevance ranking for that
call and not a stable contract.

## Review a search

The Rust client provides `Memory::review_search(ReviewSearchTarget::User { user_id },
ReviewSearchRequest::by_id(search_id))`. The HTTP equivalent is
`POST /v1/memories/search/review` with
`{"search_id":"...", "user_id":"user-123"}`. It returns the recorded query,
ranked result snapshot, and matching indexed source documents. Access is
checked against the user who ran the search.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | Tenant or ownership check failed |
| `422` | Invalid request |

## Example

```bash
curl -X POST http://127.0.0.1:8080/v1/memories/search \
  -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query": "What milk do I prefer?", "user_id": "user-123", "top_k": 5}'
```

## SDK equivalents

=== "Python"

    ```python
    search = memory.search("What milk do I prefer?", user_id="user-123", top_k=5)
    review = memory.review_search(search["search_id"], "user-123")
    ```

=== "Node.js"

    ```js
    const search = await memory.search('What milk do I prefer?', 'user-123', { topK: 5 })
    const review = await memory.reviewSearch(search.search_id, 'user-123')
    ```

=== "Rust"

    ```rust
    use membrick::{agent::{ReviewSearchRequest, ReviewSearchTarget}, Memory, SearchOptions};

    let response = memory.search(
        "What milk do I prefer?",
        "user-123",
        SearchOptions { top_k: 5, ..Default::default() },
    )?;
    let review = memory.review_search(
        ReviewSearchTarget::User { user_id: "user-123".into() },
        ReviewSearchRequest::by_id(response.search_id.clone()),
    )?;
    let hits = response.data;
    ```

Python and Node `Memory.search` return `search_id` with the `data` hit array and
provide `review_search` / `reviewSearch` for the exact query and matching source
documents. Both expose `filters`, `scope`, `reference_date`, and `options`.
`MemorySession.search` in both bindings pins
the saved session ID and accepts the same search options (apart from
`session_id`, which the session supplies). The `options` field currently has
no effect. The Rust, Python, Node.js, and HTTP client search methods return a
`search_id` with the `data` hit array. `MemoryService.search` remains the
in-process retrieval API and returns `{ data: [...] }` without a durable search
ID.
