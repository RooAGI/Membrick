# Rust SDK

Use `membrick::Memory` to connect a Rust application to a Membrick project
resident or a remote HTTP service. The resident owns the project store.

## Install

Add the crate to your application:

```sh
cargo add membrick
```

## Connect to a project

`Memory::new` connects to an existing project root. It does not create project
files. To initialize a new project intentionally, use `Memory::init`; it
creates the project configuration and returns a connected client.

```rust
use membrick::{AddOptions, Memory, Message, SearchOptions};

let client = Memory::init("./my-project")?;

let receipt = client.add(
    vec![Message {
        role: "user".into(),
        content: "I prefer tea".into(),
        timestamp: None,
        expires_at_ms: None,
        supersedes_id: None,
    }],
    "alice",
    AddOptions::default(),
)?;

let memories = client.search(
    "What does Alice prefer?",
    "alice",
    SearchOptions::default(),
)?;

println!("saved session: {}", receipt.session_id);
println!("matches: {}", memories.len());
```

For an existing project, replace `Memory::init` with
`Memory::new("./my-project")`. The path is the project root, the directory
containing `.membrick/server.toml`.

## Connect to a remote service

Use `Memory::remote` with an absolute HTTP or HTTPS URL. Pass `MemoryOptions`
to configure the request timeout or bearer token:

```rust
use membrick::{Memory, MemoryOptions};
use std::time::Duration;

let client = Memory::remote(
    "https://memories.example.com",
    MemoryOptions {
        api_key: Some("TOKEN".into()),
        request_timeout: Duration::from_secs(15),
        ..Default::default()
    },
)?;
```

For a particular resident executable, set `resident_executable` in
`MemoryOptions`. Otherwise the client resolves the executable from the
binding configuration or `PATH`.

## Use from async Rust

The client has async constructors and matching async operations. Use these
from async applications so project filesystem work and requests do not block
the executor:

```rust
use membrick::{AddOptions, Memory, Message, SearchOptions};

let client = Memory::new_async("./my-project").await?;
let receipt = client
    .add_async(
        vec![Message {
            role: "user".into(),
            content: "I prefer tea".into(),
            timestamp: None,
            expires_at_ms: None,
            supersedes_id: None,
        }],
        "alice",
        AddOptions::default(),
    )
    .await?;
let memories = client
    .search_async("What does Alice prefer?", "alice", SearchOptions::default())
    .await?;
```

Use `Memory::init_async` to initialize a project asynchronously. The other
async operations are `get_async`, `list_async`, `update_async`, `delete_async`,
and `refresh_async`. The synchronous API is also available.

## Sessions and ownership

Every memory operation requires an explicit `user_id`; identity is never
inferred from another conversation. A session is optional. If `add` receives
no `session_id`, it creates a standalone session and returns the resolved
`request_id`, `user_id`, and `session_id` in its receipt.

For a conversation across calls, keep the returned session ID or restore it
later with `client.session(user_id, session_id)`:

```rust
let conversation = client.session("alice", receipt.session_id.clone());
conversation.add(
    vec![Message {
        role: "user".into(),
        content: "Use a ceramic cup".into(),
        timestamp: None,
        expires_at_ms: None,
        supersedes_id: None,
    }],
    AddOptions::default(),
)?;
```

Session search supports follow-up resolution; it does not restrict search
results to that session. Use `ListOptions::session_id` to list one session.

## Retries and errors

Set `AddOptions::request_id` when the application must retry an add after an
uncertain result. Reuse the same ID only for the same user, session, and
messages. The client does not automatically replay an ambiguous write. Update
and delete have no operation idempotency key, so do not automatically replay
them after an uncertain outcome.

`MemoryError` distinguishes `Rejected` (the service rejected the request),
`Uncertain` (the request may have been applied), `Unavailable` (the service
could not be reached), and `Configuration` (invalid local setup or arguments).
HTTP 408 is uncertain. HTTP 429 is rejected and can be retried after backoff.

## Client lifetime

A project client and its session handles keep the resident warm while they are
open. Call `close()` to release that handle's warm lease; later calls remain
usable and acquire a lease for each request. Dropping the last handle releases
its lease automatically. The resident shuts down after its configured idle
timeout and starts again when a later request arrives.

## Agent integrations

The optional `membrick::agent` API provides typed capture and workspace search
operations for adapters. `AgentOperationContext` carries the provider,
workspace, and host session identity. Capture acknowledgements let adapters
advance transcript cursors only after durable acceptance. Host event parsing
and response formatting remain in the adapters. The read-only routes for boards
and flows are documented in the [Boards](api/boards/index.md) and
[Flows](api/flows/index.md) API references.
