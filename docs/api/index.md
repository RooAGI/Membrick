# Memory API reference

This reference documents Membrick's public memory operations across the Rust
`Memory`, embedded `MemoryService`, and the HTTP server. The MCP
memory tools are an agent-oriented adapter with inferred identity and a
different argument shape; see [MCP tools](../connect-agent.md#mcp-tools).

For the shared request, ownership, result, and retry contract across HTTP and
the Rust, Python, and Node.js clients, see [REST and Memory client contract](client-parity.md).

It covers the application-facing memory record API. Rust-only source document,
enrichment, inspection, and temporal query methods are described at the
[`MemoryService` boundary](../memory-service-api.md) and in their focused
guides below.

## Choose an interface

| Interface | Best for | Call behavior |
| --- | --- | --- |
| Rust `Memory` | Connect to a project resident or remote service | Synchronous and async methods |
| Python `Memory` | Connect to a project resident or remote service | Synchronous |
| Node.js `Memory` | Connect to a project resident or remote service | Promise-based methods |
| Rust `MemoryService` | Deliberately embed Membrick in-process | Direct service calls |
| HTTP API | Multiple processes or remote clients | Request/response |
| MCP tools | AI agents connected through MCP | Tool calls; identity may be supplied by the server |

Install the Python SDK with `pip install membrick` and the Node.js SDK with
`npm install membrick`. Rust applications depend on the `membrick` crate.

All three SDKs use the same lifecycle: `Memory(path)` connects to an existing
project without creating files, while `Memory.init(path)` explicitly initializes
project state and returns a connected client. A missing path and remote URL is a
configuration error; the client does not silently select the process working
directory. Rust applications that deliberately embed Membrick can use
`membrick::embedded::MemoryService` directly.

Project clients hold a resident lease while a client or session handle is open.
`close()` releases only that handle's warm lease and does not disable later
calls; each project request still gets a request-scoped lease. Dropping the
last handle releases the lease automatically. The resident shuts down after
its idle timeout and starts again on a later request. Remote clients do not
hold local leases. After a client process crashes, the OS releases its lock
and the resident recovers the stale lease before shutting down.
See [HTTP server setup](../server.md).

When a project client must start the resident, all language clients resolve the
CLI in the same order: explicit `resident_executable` / `residentExecutable`,
the CLI bundled with the binding package, then `membrick` on `PATH`. If no CLI
is found, project calls fail with an unavailable error before dispatch. A Rust
client never assumes its current executable is the Membrick CLI.

```python
from membrick import Memory

# Connect to the resident in this project without initializing it
memory = Memory(path=".")

# Initialize a new project only when explicitly requested
new_project = Memory.init(path="./my-project")

# Or connect to a remote server
remote = Memory(base_url="http://127.0.0.1:8080", api_key="TOKEN")
```

Use `path` in Python and Node to select a project root. It connects to that
project's resident and requires existing `.membrick/server.toml` configuration.
Call `Memory.init(...)` only when you intend to initialize project state.
For remote connections, provide `base_url` or `baseUrl`.

```js
import { Memory } from 'membrick'

const memory = new Memory({ path: process.cwd() })
// Initialize project state only when the application intends to do so:
const fresh = Memory.init({ path: './my-project' })
// Or: new Memory({ baseUrl: 'http://127.0.0.1:8080', apiKey: 'TOKEN' })
```

Node constructor options and method parameters use camelCase (`baseUrl`,
`requestTimeoutMs`, `userId`, `sessionId`). Message and returned record objects
retain the Rust/HTTP wire field names in snake_case (`expires_at_ms`,
`session_id`, `user_id`). This distinction keeps JavaScript call ergonomics
while preserving JSON-compatible request and response shapes.

## Language support

| Capability | Rust | Python | Node.js |
| --- | --- | --- | --- |
| Resident and remote `Memory` client | Yes | Yes | Yes |
| Direct embedded `MemoryService` | Yes | No | No |
| `Mem0Adapter` | No | Yes, documented subset | No |
| Typed agent capture and workspace search | Yes, under `membrick::agent` | No | No |

The Python Mem0-style adapter is not drop-in Mem0 compatibility; see the
[supported contract](../mem0-compat.md). Agent integrations continue to handle
host-specific event parsing and response formatting around the Rust typed
operations.

## Operations

- [Add](add.md) one or more messages. Native client calls require messages and
  `user_id`; request and standalone session IDs are generated when omitted.
- [Search](search.md) for relevant memories.
- [Get](get.md) one memory by its opaque ID.
- [List](list.md) a user's memories with cursor pagination.
- [Update](update.md) an existing active memory.
- [Delete](delete.md) a memory.
- [Refresh](refresh.md) pending index changes when using a lower-level or
  batched workflow.

## Identity and IDs

`user_id` scopes memory ownership. The same user ID must be used to read,
update, or delete that user's memories. When the HTTP server uses bearer
authentication, the authenticated identity is authoritative and the server
checks any supplied user ID against it.

The native client generates a standalone `session_id` on add unless provided.
A session handle preserves user and session IDs across calls. Sessions group
messages from the same conversation.
On search, it optionally enables follow-up query resolution from bounded
conversation state. It does not filter search results to only that session.
Use `session_id` on `list` when you specifically want to list one session.

`request_id` is an idempotency key for an add request, scoped to a user. Retry
an uncertain write with the same request ID and identical session/messages.
The same ID with different content or a different session is rejected. Do not
use a new ID just to retry a request whose result is unknown.

Memory IDs are opaque. Obtain them from `add`'s adjudication receipt,
`search`, `get`, or `list`; do not construct IDs yourself.

## Time values

`timestamp` and `expires_at_ms` are Unix timestamps in milliseconds.
`timestamp` is signed; `expires_at_ms` is a non-negative integer. `created_at`
in returned records is an RFC 3339 string when available.

## HTTP conventions

The canonical HTTP routes are listed in each operation page. Send JSON with
`Content-Type: application/json`. If the server is configured for bearer
authentication, send `Authorization: Bearer <token>`. Writes may be accepted
before the new search snapshot is visible; add supports
`?wait_for_visibility=true` when the caller needs the updated view before the
response returns. The server generates `request_id` when omitted; provide a
stable ID when a write must be safely retried after an uncertain response.
See the [HTTP API overview](http-overview.md) for authentication modes,
response limits, and status codes.

## Current surface differences

| Capability | Rust | Python | Node.js | HTTP |
| --- | --- | --- | --- | --- |
| Add batch | Yes | `add_batch` | `addBatch` | No batch endpoint |
| Search `filters`, `scope` | Yes | Yes | Yes | Yes |
| Search `reference_date` and `options` | Yes | Yes | Yes | Yes |
| Supersede / expire | `MemoryService` | No | No | No |

Python, Node.js, and Rust `Memory.search` methods return the search hit
array directly. HTTP responses and `MemoryService.search` use a `{ "data": [...] }`
wrapper. The `options` field is reserved and currently has no effect. Python
and Node.js session search pin the session ID and otherwise accept the same
search options as their client-level search.

## Related references

- [HTTP API overview](http-overview.md): authentication, errors, and every
  route group
- [Python binding](../python.md)
- [Node.js binding](../node.md)
- [Rust Memory API](../memory-client-rust.md)
- [HTTP server](../server.md)
- [MCP tools](../connect-agent.md#mcp-tools)
- [Mem0-style Python adapter](../mem0-compat.md): the documented subset and
  behavior differences from Mem0
- [Rust `MemoryService` boundary](../memory-service-api.md)
