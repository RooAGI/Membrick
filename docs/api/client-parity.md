# REST and Memory client contract

The REST memory API and the Rust, Python, and Node.js `Memory` clients share
the `/v1/memories` contract. The language bindings map values and errors; they
do not implement a separate memory service. This page records where HTTP
request fields live and how their results correspond to client return values.

Provider memory, capture, dashboard, boards, and flows are separate workspace
APIs. They are not aliases for the application's `Memory` operations.

## Operation mapping

| Operation | REST | Rust `Memory` | Python `Memory` | Node.js `Memory` |
| --- | --- | --- | --- | --- |
| Add | `POST /v1/memories` | `add(messages, user_id, AddOptions)` | `add(messages, user_id, request_id=None, session_id=None, wait_for_visibility=False)` | `add(messages, userId, options?)` |
| Search | `POST /v1/memories/search` | `search(query, user_id, SearchOptions)` | `search(query, user_id, top_k=10, ...)` | `search(query, userId, options?)` |
| Get | `GET /v1/memories/{id}?user_id=...` | `get(id, user_id, GetOptions)` | `get(memory_id, user_id, include_inactive=False)` | `get(memoryId, userId, includeInactive?)` |
| List | `GET /v1/memories?user_id=...` | `list(user_id, ListOptions)` | `list(user_id, session_id=None, limit=100, cursor=None, include_inactive=False)` | `list(userId, options?)` |
| Update | `PATCH /v1/memories/{id}` | `update(id, user_id, content, UpdateOptions)` | `update(memory_id, user_id, content, ...)` | `update(memoryId, userId, content, options?)` |
| Delete | `DELETE /v1/memories/{id}?user_id=...` | `delete(id, user_id)` | `delete(memory_id, user_id)` | `delete(memoryId, userId)` |
| Refresh | `POST /v1/memories/refresh` | `refresh()` | `refresh()` | `refresh()` |

Field names in JSON are snake_case. Node method parameters and option objects
use camelCase; the Node binding translates them at the boundary. Python uses
snake_case. Rust uses typed option structs. In all interfaces `user_id` is
required for normal memory operations, including add and search; identity is
never inferred from the most recently active conversation.

## Add identity and retry behavior

`user_id` is required. `session_id` is optional: when omitted, the server
resolves a standalone session and returns it in the add receipt. An explicit
session is preserved. Pass the returned session to a `Memory.session(...)`
handle (or construct a Rust session) to continue a conversation across calls.

`request_id` is optional for a one-shot call. The server generates it when it
is absent and returns it in the receipt. If the caller must recover from a
lost HTTP response, send its own durable `request_id` and retry with exactly
the same owner and payload. If `session_id` is omitted with an explicit ID,
the server resolves a stable session from `(user_id, request_id)`, so replaying
the same request does not create another conversation or duplicate the write.
If `session_id` is explicit, preserve it too. Reuse of an explicit request ID
with changed messages or session is rejected. Never generate a new request ID
when retrying an operation whose outcome is unknown.

The native clients generate request and session IDs once before dispatch and
retain those values across retries internal to one call. Applications that
need durable retries across process restarts should supply the same explicit
`request_id` on the next call, and should preserve any explicit `session_id`.
The successful receipt exposes both resolved IDs.

## Results and missing records

The REST and native clients use the same record fields (`id`, `content`,
`role`, `user_id`, `session_id`, timestamps, expiry, supersession, and
metadata). Add returns a receipt containing `success`, `request_id`,
`user_id`, `session_id`, and optional adjudication details. Search returns
`{ "data": [...] }` over REST; Rust returns a typed `SearchResponse` and the
Python and Node convenience methods return the hit array. List returns
`{ "data": [...], "next_cursor": ... }` in every language.

Get returns the record on success. A missing, inactive, or foreign-owned
record is HTTP `404`; the SDKs map that to `None`/`null`. Update has the same
not-found mapping. Delete returns HTTP `204` when removed and `404` when not
found; SDKs map those to `true` and `false`. Refresh returns HTTP `204` and
maps to `None`/`undefined` in the language bindings.

## Ownership and authentication

Every operation is scoped by its explicit `user_id`. For get, update, and
delete the ID in the path identifies a record, but does not grant access to
it. The server checks the requested owner against the record owner. When
bearer authentication is enabled, the authenticated tenant/user is also
checked against the supplied owner. A mismatch is rejected; clients cannot
use a filter or session to widen ownership. See
[HTTP authentication](http-overview.md#authentication).

## Errors and retry decisions

The native clients expose typed error categories: `Rejected` means the server
returned a definitive rejection; `Unavailable` means a service could not be
reached before dispatch; `Uncertain` means dispatch may have happened but the
client could not confirm the result; and `Configuration` means local client
configuration is invalid. Python and Node preserve these categories in their
language-specific error types/codes. HTTP validation and ownership failures
are rejections. HTTP `408` is uncertain because it does not prove the write
was not accepted. HTTP `429` is a definitive queue-admission rejection in
this API and can be retried after backoff with the same request ID.

After `Uncertain`, retry a write only with the original `request_id` and
identical payload. A `Rejected` validation or ownership error should be fixed
before retrying. An `Unavailable` result can be retried with the same
request ID and payload; doing so is safe for writes. Read requests can be
repeated normally. `refresh` has no request ID and only asks the service to
publish already accepted writes.

For REST, a reused `request_id` with changed content or session returns `409`
in the current server; earlier versions returned `422`. REST clients that
inspect this status need to handle `409`. Native clients classify either
status as `Rejected` because both are definitive 4xx responses.

## Wire routes outside this contract

`/provider-memory/*`, capture, and dashboard routes serve provider or workspace
operations. Their caller identity and result contracts are different from the
`Memory` client API. Applications should use the versioned `/v1/memories`
routes above.
