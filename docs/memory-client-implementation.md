# Memory implementation plan

## Goal

Provide one public Rust `Memory` for Rust applications, Python, Node.js,
and agent integrations. The client prepares requests, owns identity and session
context, and selects a local resident or remote HTTP transport. The server owns
`MemoryService`, which remains responsible for validation, authorization,
memory lifecycle, persistence, indexing, and search behavior.

```text
Rust callers   Python binding   Node binding   Agent adapters
     └──────────────┴──────────────┴──────────────┘
                         Memory
                               ↓
                  Local resident / HTTP service
                               ↓
                         MemoryService
```

This document records the implemented architecture and the staged migration.
Rust, Python, and Node expose one `Memory` client for resident and remote
connections. Python and Node do not expose an embedded-path binding. Rust
applications that deliberately embed Membrick use `MemoryService` directly. See the [Memory API reference](api/index.md),
[`MemoryService` boundary](memory-service-api.md), and [server guide](server.md).

The Rust contract and transport increment exposes project and remote
constructors, typed memory operations, explicit sessions, and typed rejected,
uncertain, unavailable, and configuration errors. Rust is synchronous. The
project transport uses the resident channel for all supported HTTP methods and
retains add's durable queue fallback. Add IDs are stable for the prepared
request; callers may supply an ID for durable retry. Ambiguous calls are not
automatically replayed. See the [Rust Memory API](memory-client-rust.md)
for the current methods and retry guarantees.

## 1. Client contract

Expose `add`, `search`, `get`, `list`, `update`, `delete`, and `refresh` as
typed operations. Define typed request options, responses, and errors in Rust;
bindings map those values to their native conventions. Constructors must make
connection intent explicit:

```rust,ignore
let client = Memory::new(project_root)?;
let client = Memory::remote(base_url, options)?;
```

The Rust connection and operation surface is synchronous. Async Rust support
remains a separate design decision; later bindings must preserve their
documented synchronous/asynchronous execution models.

Exact constructor names may change during implementation, but project/local
resident selection and remote HTTP configuration must remain distinct in the
public API. Retain direct `MemoryService` constructors for applications that
intentionally embed Membrick.

Ordinary calls require ownership context. The user is explicit on each
operation or inherited from an explicitly constructed user-scoped session;
the client must never infer a user from a previous or most recently active
conversation. Session creation is optional for callers:

```python
client.add(messages, user_id="alice")
client.search("What are Alice's preferences?", user_id="alice")
```

The Python API should also provide an explicit session object for conversations
that span calls. Session values preserve caller-supplied IDs and ownership
context; restoring a session from its ID must not create a new identity.

## 2. Identity, receipts, and retry semantics

For a write, generate one request ID before transport dispatch and preserve it
unchanged through every retry. Allow applications to supply a durable request
ID when they own retry scheduling. Preserve explicit session IDs. If `add`
omits a session, generate a standalone session ID internally to meet the
existing service contract. Return the resolved request, user, and session IDs
in the add receipt so callers can persist or correlate them.

An operation error must distinguish a definitive rejection from an uncertain
outcome (for example, a timeout after the service may have committed a write).
Automatic retries are enabled only for operations whose replay behavior has
been audited. In particular, a retry after an uncertain add must use the same
request ID and identical payload and ownership/session context. A changed
payload with a reused ID must be rejected by the service. Do not automatically
replay update or delete until their idempotency and response-loss semantics are
specified. Read retries may use bounded retry policies because they do not
commit writes.

## 3. Shared transports

Move duplicated Python and Node HTTP request code into the Rust client. Place
the existing resident connection machinery behind the same client boundary:
resident startup coordination, IPC, configured HTTP fallback, file queue,
leases, and timeout handling. Bindings must not independently select or retry
transports.

The resident remains the owner of `MemoryService`. Project clients connect to
the project resident rather than opening the resident's persistent store as a
second writer. Remote clients use the versioned HTTP API. Transport fallback
and queue behavior must preserve request IDs and report whether the operation
was rejected or may have committed.

Before enabling automatic write retries, document and verify the replay
contract for each operation and transport, including recovery from a lost
response after commit. Existing add idempotency is the initial basis; it is not
proof that every write can be replayed safely.

## 4. Rust API and ownership boundary

Expose the same operations and session behavior to Rust callers through
`Memory`. Keep `MemoryService` available for deliberate embedded use.
The client is not a replacement domain layer: validation, persistence,
indexing, lifecycle rules, and authorization enforcement remain in
`MemoryService` and the server. The server owns its service instance; client
constructors must not create competing service owners for one project store.

The existing service methods are synchronous. The shared client transport and
binding strategy must be resolved before implementation: an async Rust client
fits Node promises and IPC/HTTP work, while Python currently offers synchronous
methods and embedded service calls are synchronous. Choose a supported
blocking facade or runtime handoff for Python and document how it avoids
blocking an async host. Keep DTOs independent of PyO3/napi-rs types.

## 5. Language bindings

Once Rust contract and transports are usable, make Python and Node delegate to
`Memory`. Their remaining responsibilities are conversion between
language values and Rust DTOs, mapping Rust errors, and scheduling synchronous
or asynchronous execution. Node methods return promises. Python initially
retains its synchronous facade unless a separate async surface is deliberately
added.

The public Python and Node API has one `Memory` class with message-oriented
calls. Existing embedded `path` use must migrate to the project resident or a
remote service. Rust callers that deliberately embed use `MemoryService`.

## 6. Agent integrations

Migrate Codex and Claude first, then the remaining integrations. Agent adapters
continue parsing host events and formatting host responses; shared operations
move into typed client APIs for capture, workspace search, boards, and flows.

Preserve host session IDs, provider attribution, workspace boundaries, capture
cursors, and durable acknowledgements. Capture recovery must resume from the
last acknowledged cursor without dropping or duplicating acknowledged events.
Board and flow requests must use the same project/ownership boundaries as
memory operations. The client should not make adapters reconstruct transport
policy or infer ownership from recent activity.

## 7. Mem0 compatibility

Use familiar `add` and `search` calls in the native client, but treat exact
Mem0 compatibility as a separate adapter built over `Memory`. Before
calling it compatible, document the supported arguments, filters, return
shapes, errors, and behavior differences. Test that defined contract. Until
then, describe the API as Mem0-inspired where relevant, not a drop-in
replacement.

## 8. Delivery sequence and acceptance gates

Each stage is a reviewable change and must be usable and verified before the
next consumer migrates.

| Stage | Deliverable | Gate before proceeding |
| --- | --- | --- |
| 1. Rust contract and transports | Typed `Memory`, explicit project and remote constructors, resident/HTTP transport path, typed errors and sessions | Rust callers can connect through both modes; request IDs survive transport retries; rejected and uncertain outcomes are distinguishable |
| 2. Memory operations | Client implementations of add, search, get, list, update, delete, refresh | Operation contracts and replay rules are documented; duplicate prevention after a lost add response and session restoration work |
| 3. Language bindings | Python and Node delegate to the client; retain migration aliases | Equivalent Rust/Python/Node operations return equivalent records and receipts; user/project isolation holds |
| 4. Agent integrations | Codex and Claude, then remaining adapters; typed capture, workspace search, boards, flows | Capture recovery, host session preservation, workspace boundaries, provider attribution, and durable acknowledgements hold |
| 5. Compatibility adapter | Documented Mem0-compatible subset over the native client | Contract tests cover every advertised argument, filter, result, error, and documented difference |

The verification matrix across these changes must cover:

- Duplicate prevention when a committed write response is lost.
- Session restoration and preservation of explicit session IDs.
- User and project isolation, including concurrent clients.
- Resident restart and recovery of queued or uncertain operations.
- Capture cursor recovery and durable acknowledgements.
- Equivalent Rust, Python, and Node results for shared operations.
- Retry limits and behavior for definitively rejected versus uncertain writes.

Do not migrate a consumer to behavior that has not passed its stage gate. Keep
each stage's examples and the [Quickstart](../README.md) aligned with the
surfaces actually released.

## Open implementation decisions

These are implementation questions, not reasons to weaken the contract:

1. Select the public async Rust runtime strategy and the Python synchronous
   handoff, accounting for the synchronous `MemoryService` embedding path.
2. Define the stable typed error taxonomy, including transport unavailable,
   rejected operation, uncertain outcome, authorization failure, and invalid
   request.
3. Specify which non-add writes are idempotent and whether the service needs
   operation-scoped idempotency keys before automatic retry is enabled.
4. Specify project client startup, resident discovery, IPC framing/versioning,
   and file-queue durability using the existing connection machinery.
5. Define typed capture, workspace search, board, and flow DTOs and map them to
   current server/MCP contracts without moving domain validation into adapters.
6. The Python and Node bindings expose one `Memory` class for project/resident
   and remote connections. They do not expose an embedded-store class or a
   `path` constructor. Rust applications that deliberately embed Membrick use
   `MemoryService` directly. `Memory.add` accepts an optional explicit
   `request_id` for durable retries; applications should migrate from the old
   positional request/user/session form to `add(messages, user_id, ...)`.
