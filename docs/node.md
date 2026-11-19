# Node.js SDK

For method-by-method request and response contracts, see the
[Memory API reference](api/index.md).

The `membrick` package exposes the shared Rust `Memory` to Node.js.
`Memory` connects to the project resident or to a remote HTTP service.

Install it from npm:

```bash
npm install membrick
```

The package supports Node.js 18 and newer. Prebuilt packages are provided for
macOS arm64/x64, Linux arm64/x64 with glibc, and Windows x64.

The package also installs the `membrick` CLI for the supported platform. For
example, install it globally and connect a project to Codex:

```bash
npm install --global membrick
membrick install codex .
```

The CLI is the same Rust executable shipped in Membrick releases. The npm
platform package includes both that executable and the Node native binding.

## Unified client

Pass `path` to connect to an existing project's resident, or `baseUrl` to
connect remotely. Construction does not create `.membrick` or server settings.
To initialize a new project intentionally, call `Memory.init({ path })`.
Omitting both `path` and `baseUrl` is a configuration error;
the SDK does not silently use the process working directory.

```js
const { Memory } = require('membrick')

const memory = new Memory({ path: process.cwd() })
// Initialize project state only when the application explicitly asks for it.
const fresh = Memory.init({ path: './my-project' })
const receipt = await memory.add(
  [{ role: 'user', content: 'I prefer oat milk.' }],
  'alice',
)
const search = await memory.search('What are Alice’s preferences?', 'alice')
console.log(search.search_id, search.data)
const review = await memory.reviewSearch(search.search_id, 'alice')
```

`search` searches user-scoped memories. To search captured memories and
workspace memory, call `workspaceSearch` with the provider identity. A project
connection uses its connected root as the workspace; a remote connection must
pass `workspace` explicitly:

```js
const matches = await memory.workspaceSearch(
  'What did we decide about the API?',
  'codex',
  { topK: 5 },
)
const review = await memory.reviewSearch(matches.search_id, undefined, {
  provider: 'codex',
})
```

`reviewSearch` also handles workspace searches. Pass workspace options with a
`provider` instead of passing a `userId`; include `workspace` and `sessionId`
when needed. To page through unreviewed workspace searches, call
`reviewSearch(undefined, undefined, { provider, cursor, pageSize })`.

Pass `requestId` when the application needs to reuse an idempotency key after
an uncertain response. `memory.session(userId, sessionId)` creates a view that
keeps those identities fixed across calls. Node methods return promises; the
native async binding executes the synchronous Rust client work away from the
JavaScript event loop, though long calls still occupy a native worker.

Errors carry a `code` property so callers can distinguish a rejected request
from an unavailable service or uncertain result. Use `isMemoryError` and the
exported `MemoryError` type to narrow caught values; HTTP rejections also
include `status`:

```js
const { Memory, isMemoryError } = require('membrick')

try {
  await memory.add([{ role: 'user', content: 'I prefer concise answers.' }], 'alice')
} catch (error) {
  if (isMemoryError(error)) {
    console.error(error.code, error.status)
  }
  throw error
}
```

JavaScript option names and method parameters use camelCase, such as
`baseUrl`, `requestTimeoutMs`, `userId`, and `sessionId`. Message objects and
returned memory records keep their snake_case wire names, such as
`expires_at_ms`, `user_id`, and `session_id`, so they retain the same JSON field
shape as Rust and Python values.

## Remote memory service

The new client's `baseUrl` option connects directly to a remote service:

```js
const memory = new Memory({
  baseUrl: 'http://127.0.0.1:8080',
  apiKey: process.env.MEMBRICK_API_KEY,
})
const search = await memory.search('When is the launch?', 'alice')
const review = await memory.reviewSearch(search.search_id, 'alice')
const results = search.data
```

The remote backend uses the versioned `/v1/memories` HTTP API. `Memory` is the
single supported resident and remote SDK client. Methods return promises so
Rust work and HTTP requests do not block the JavaScript event loop.
In project mode, resident executable lookup is consistent across Rust, Python,
and Node: `residentExecutable` takes priority, then the matching CLI bundled
with the language package, then `membrick` on `PATH`. If none is available,
the call fails with an `UNAVAILABLE` error before dispatch. The client never
attempts to launch the Node interpreter as a resident.

A project client keeps the resident warm while it is open. Sessions hold their
own reference, so closing the client does not release the resident while a
session is still open. Call `close()` when a handle no longer needs to keep the
resident warm; it is idempotent and later calls remain usable with
request-scoped leases. Dropping the last client or session also releases the
lease. Once all local client and provider adapter leases close, the resident
flushes pending writes and shuts down. A later request starts it again. Remote
clients need no local lease.

```js
const session = memory.session('alice', savedSessionId)
memory.close() // The session still keeps the resident warm.
await session.search('What did Alice prefer?')
session.close() // Idle shutdown can proceed when no other client is open.
```


See [`bindings/node/README.md`](https://github.com/RooAGI/Membrick/tree/main/bindings/node)
for source builds and package details.
