# Search Membrick memories

Search lets an agent or application find useful project context from a natural
language question. Membrick ranks matching memories and source material, and
returns evidence with each hit so the caller can judge where an answer came
from.

## Search from an agent

When an agent is connected through MCP, it can call `search` with a question.
The connection scopes the search to the current project. Lifecycle hooks can
also retrieve context automatically when the agent starts work or reaches a
configured event.

See the [MCP tool reference](mcp-memory-server.md) and the guide to
[automatic recall and capture](agent-memory.md).

## Search from an application

The Python, Node.js, and Rust clients use the same `Memory` API. Each search
requires an owner ID; in project mode this keeps one user's memories separate
from another's.

=== "Python"

    ```python
    hits = memory.search("How should answers be written?", user_id="alice", top_k=5)
    for hit in hits:
        print(hit.content, hit.score)
    ```

=== "Node.js"

    ```js
    const hits = await memory.search(
      'How should answers be written?',
      'alice',
      { topK: 5 },
    )
    for (const hit of hits) console.log(hit.content, hit.score)
    ```

=== "Rust"

    ```rust
    let hits = memory.search(
        "How should answers be written?",
        "alice",
        SearchOptions { top_k: 5, ..Default::default() },
    )?;
    for hit in hits {
        println!("{} ({:.2})", hit.content, hit.score);
    }
    ```

For HTTP, send `POST /v1/memories/search`. The request and response fields are
listed in the [Search API reference](api/search.md).

## Search options

- `top_k` limits how many results are returned. It defaults to 10.
- `filters` narrows results by indexed metadata when the selected interface
  supports those fields.
- `reference_date` anchors relative date phrases such as “last month.”
- `session_id` supplies recent conversation context to resolve follow-up
  questions. It does **not** restrict results to that session. To retrieve
  memories from one session, use the [list operation](api/list.md) with a
  `session_id` filter.
- `scope` separates conversation state from the owner ID. It does not grant
  access to another owner's memories.

## Read the results

Each hit includes its content, a relative relevance score, and available source
metadata. Some results also include relation evidence or claim-state details.
Use those fields to verify the source and whether a claim is current. Ranking
can change as the project gains or updates information, so result order is not
a stable identifier.

For API schemas and examples, see [Search memories](api/search.md).
