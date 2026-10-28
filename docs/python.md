# Python SDK

The `membrick` Python package exposes the shared Rust `Memory` client. It
connects to an existing project resident or to a remote HTTP service.

## Install

```bash
pip install membrick
```

The package includes the native binding and the Membrick CLI for supported
platforms. You do not need to install or start a separate resident executable.

## Connect to a project

Pass the project root to connect to its resident. Construction does not create
project files. Use `Memory.init(...)` only when you intentionally want to
initialize Membrick state:

```python
from membrick import Memory

memory = Memory(path=".")
receipt = memory.add(
    [{"role": "user", "content": "I prefer concise answers."}],
    user_id="alice",
)
hits = memory.search("How should answers be written?", user_id="alice")
print(hits)
```

An add receipt includes the resolved `request_id`, `user_id`, and `session_id`.
Keep the session ID to continue a conversation across calls:

```python
conversation = memory.session("alice", receipt["session_id"])
conversation.add([{"role": "user", "content": "Use bullet points."}])
hits = conversation.search("What answer format do I prefer?")
```

To create a new project intentionally, initialize it first:

```python
new_memory = Memory.init(path="./my-project")
```

Call `close()` on clients and sessions when they no longer need to keep the
resident warm. Dropping the final handle releases its lease automatically.
Later requests can start the resident again.

## Connect to a remote service

Set `base_url` to use the HTTP API. Provide `api_key` when the server requires
token authentication:

```python
remote = Memory(
    base_url="http://127.0.0.1:8080",
    api_key="YOUR_SERVER_TOKEN",
)
hits = remote.search("When is the launch?", user_id="alice")
```

`Memory` is the same client for project and remote connections. Omitting both
`path` and `base_url` is a configuration error; the package does not silently
select the current directory.

## IDs and errors

Every call takes an explicit `user_id`. The client generates a request ID for
each new add. If the application persists request IDs to recover from an
uncertain response, reuse the same ID only for the same user, session, and
messages. `MemoryError` subclasses retain useful built-in exception types:

| Exception | Meaning |
| --- | --- |
| `RejectedError` | The service definitively rejected the operation; includes `.status` for HTTP responses. |
| `UnavailableError` | The service was unavailable before dispatch. |
| `UncertainError` | The request may have been applied; check or retry with the same request ID. |
| `ConfigurationError` | The client options or project setup are invalid. |

The synchronous binding releases the Python GIL while the client performs
service work. See the [shared Memory API contract](api/index.md) for operation
signatures and return values, and the
[Mem0-style adapter guide](mem0-compat.md) for its supported Python subset.
