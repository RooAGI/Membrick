# Memory client architecture

Membrick exposes one application client named `Memory` in Rust, Python, and
Node.js. The language bindings use the shared Rust implementation for
connection and request behavior; they translate values and errors at the
language boundary.

## Connections

`Memory` connects either to a project's resident or to a remote HTTP service:

- **Project:** pass an existing project root. The client talks to its resident;
  the resident owns `MemoryService` and is the sole writer for that project.
- **Remote:** pass a `base_url`/`baseUrl` to connect to a Membrick HTTP service.
- **Initialize:** call `Memory.init(...)` only when you intend to create
  Membrick project state. A normal constructor does not initialize a project
  or silently choose the current directory.

Rust applications that intentionally embed Membrick can use
[`embedded::MemoryService`](memory-service-api.md) directly. This is separate
from the resident-backed `Memory` client used by project applications.

## Operations and identity

The client provides `add`, `search`, `get`, `list`, `update`, `delete`, and
`refresh`. Rust offers synchronous and asynchronous methods; Python exposes
synchronous calls and Node.js exposes promise-based calls. The bindings convert
language values and errors while using the shared Rust client implementation.
Calls that access a user's memories take an explicit `user_id`; the client
never infers ownership from another conversation.

Each new add gets a request ID. Applications that need to recover from an
uncertain response can provide and persist their own ID, then reuse it only
when retrying the same operation. If an add omits `session_id`, the client
resolves a standalone session ID and returns it in the receipt.

A session handle keeps the user and session IDs fixed across multiple calls.
Restore one later with the IDs returned by add or stored by the application.
Supplying a session ID to search enables follow-up query context; it does not
restrict the search results to that session. Use list with a session filter for
that purpose.

## Resident lifetime and errors

An open project client or session holds a lease that keeps its resident warm.
The lease lasts for the handle's lifetime, not just for one request. Call
`close()` to release it explicitly; dropping the last handle releases it
automatically. Remote HTTP clients do not hold a project-resident lease. After
all local leases end, the managed resident can shut down after its configured
idle timeout. A later project request starts it again.

Errors distinguish a rejected operation, an uncertain outcome, an unavailable
service, and invalid configuration. Treat uncertain writes as possibly
applied: inspect the result or retry an add with the same request ID.

## Language guides

- [Rust `Memory`](memory-client-rust.md)
- [Python `Memory`](python.md)
- [Node.js `Memory`](node.md)
- [Shared REST and client contract](api/client-parity.md)
