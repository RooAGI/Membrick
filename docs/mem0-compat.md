# Mem0 Platform Python adapter

## Contract version

The compatibility target is the **Mem0 Platform Python SDK `mem0ai==2.2.1`**
as checked on 2026-10-27. This is distinct from Mem0's OSS `Memory` API and
from calling the Platform REST endpoints directly. The Python adapter remains
an opt-in wrapper around Membrick `Memory`; it does not currently replace
`from mem0 import MemoryClient` construction or authentication.

`Mem0Adapter` provides a Mem0 Platform SDK-shaped subset over a `Memory`
client. This contract targets the Python/JavaScript calls and synchronous
result examples in the current [Platform Quickstart](https://docs.mem0.ai/platform/quickstart),
not the lower-level REST workflow as a whole. It is **not a drop-in replacement**.

In Python SDK v2.2.1, `add(messages, options=None, **kwargs)` and
`update(memory_id, options=None, **kwargs)` reserve the second positional
argument for a typed options object. This adapter requires `user_id` for
ownership and keeps a narrower signature: `add` accepts a string, one message
dict, or a list; `update` accepts named `text` (or legacy `data`) and named
`user_id`. Use `update(memory_id, text="...", user_id="...")`; do not pass a
plain string as the second positional argument.

## Platform Python add behavior

For the Platform Python SDK v2.2.1, `add(..., infer=True)` uses asynchronous
processing and returns `{ "event_id": "...", "status": "PENDING" }`; callers
poll the event endpoint for its terminal result. With `infer=False`, the SDK
uses synchronous storage and returns results/message. The Platform REST
[Add Memories endpoint](https://docs.mem0.ai/api-reference/memory/add-memories)
documents the asynchronous event workflow. This adapter currently implements
neither Platform event polling nor Mem0's `infer=False` response contract: it
waits for Membrick visibility and returns stored records. The adapter defaults
`infer` to `False` to support the common `add(messages, user_id=...)` call;
explicit `infer=True` is rejected because Mem0 inference semantics are not
implemented. `request_id` is optional and generated when omitted, but callers
must supply and reuse it to make a retry durable across calls. The OSS Mem0 API's
`{"results": [...]}` result contract is a separate interface and is not the
target here.

The request contract was checked against the official Platform docs and
Python SDK version on 2026-10-27. Re-check and update this contract before
claiming compatibility with another SDK release.

## Usage

```python
from membrick import Memory, Mem0Adapter

client = Memory(path=".")  # Connect to this project's resident
memory = Mem0Adapter(client)

added = memory.add(
    [{"role": "user", "content": "I prefer oat milk."}],
    user_id="alice",
)
# {"results": [{"id": "...", "memory": "I prefer oat milk.", ...}]}

hits = memory.search("What milk does Alice prefer?", user_id="alice")
# {"results": [{"id": "...", "memory": "I prefer oat milk.", ...}]}
```

## Supported subset

Writes and single-record operations require explicit `user_id`. Search and
`get_all` accept `user_id` inside `filters`, matching Platform's call shape.
The adapter never infers ownership from a previous call or the most recent
conversation. `run_id` is a Membrick extension mapped to `session_id` on add.
It is returned on add when explicitly supplied. On search, it supplies
follow-up-query context and does not restrict results to that session. A
search hit receives `run_id` only when its stored `session_id` matches the
requested run; hits from other sessions are not relabeled. `get_all` returns
`run_id` when a run filter is supplied. Native `session_id` is also included
in records; generated request-scoped session IDs are never relabeled as
`run_id`.

| Call | Adapter result | Differences from Platform |
| --- | --- | --- |
| `add(messages, user_id, run_id=None, ..., infer=False, expiration_date=None, request_id=None)` | Membrick-specific envelope `{"results":[{"id","memory", ...}]}`. One result per stored message document. Add waits for the service visibility receipt. `expiration_date` is accepted as a UTC calendar date and stored as the exclusive start of the following UTC day, so the record stays visible through the named date; the result maps that boundary back to the requested date. A supplied message `expires_at_ms` must equal this exclusive boundary. `request_id` is a Membrick extension for durable retries and is generated when omitted; reuse an explicit ID across calls to retry safely. If supplied without `run_id`, the adapter derives a stable request-scoped session ID and omits it from returned `run_id`. | The common `add(messages, user_id=...)` call is accepted because `infer` defaults to `False`; this differs from Platform's inference-enabled default. The current REST endpoint returns async `{event_id,status}` and requires event polling, which this adapter does not implement. Membrick stores submitted message documents and separately reports adjudication; it does not extract or split facts. It omits Mem0's `event` field because an idempotent replay does not establish whether Mem0 would report ADD or UPDATE. Membrick IDs are opaque document IDs, not UUIDs. |
| `search(query, user_id=None, top_k=10, filters=None, run_id=None, ...)` | `{"results":[...]}` with `id`, `memory`, `user_id`, `metadata`, and available `run_id`, `session_id`, `created_at`, `expiration_date`, `score`. `user_id` may come from `filters`; `run_id` may also be specified there. | Supports nested `AND`/`OR`/`NOT`, `eq`, `ne`, `in`, `gt`, `gte`, `lt`, `lte`, `contains`, wildcard existence, and metadata object subset matching. Filters run before ranking and `top_k` selection. `top_k` is limited to 100 (`limit` remains an alias). Agent/app scope, threshold, rerank, `show_expired`, categories, and exact ranking semantics are unsupported. |
| `get_all(filters, page=1, page_size=50)` | Mem0-shaped `{count,next,previous,results}` envelope. `filters` must contain `user_id`; nested filters and optional `run_id` are supported. | Links use `membrick://` because this client does not expose a Mem0 HTTP endpoint. Entity scopes other than `user_id`/`run_id` and `show_expired=True` are rejected. Filtering happens before count and pagination. This preserves response keys and page behavior, not Mem0's hosted URLs or complete filter contract. |
| `get(memory_id, user_id)` | Record with `id`, `memory`, `user_id`, `metadata`, native `session_id`, and available `created_at`, `expiration_date` (`YYYY-MM-DD`); missing/foreign IDs raise `ValueError` with a 404 message. | This adapter does not map `session_id` to `run_id` for get. Platform records can include categories and agent/app scope, which this adapter does not provide. |
| `update(memory_id, text, user_id, metadata=None)` | Updated record in the same shape as `get`; metadata and existing expiration are preserved when omitted, while supplied metadata replaces user metadata. The legacy `data=` keyword remains accepted. Missing/foreign IDs raise `ValueError` with a 404 message. | Platform uses `text=` and does not take `user_id`; Membrick requires that explicit owner extension. Metadata is serialized losslessly as JSON inside the index's string-valued filter field and decoded in adapter results. The adapter does not accept a new expiration value on update and does not provide `updated_at`, categories, or linked state. |
| `delete(memory_id, user_id)` | `{"message":"Memory deleted successfully!"}`; missing/foreign IDs raise `ValueError` with a 404 message. | Platform can delete linked memories and return `cascade_count`; adapter deletes only the selected memory. |

`search` defaults `top_k` to 10 and accepts the legacy `limit` alias. Pass one
or the other; if both are explicitly supplied, the adapter raises `ValueError`
even when `top_k=10` equals its default. `get_all` walks service cursor pages,
counts all matching records for the Platform envelope, and retains only the
requested result page in memory.

The Platform CRUD reference examples are [Search Memories](https://docs.mem0.ai/api-reference/memory/search-memories),
[Get Memory](https://docs.mem0.ai/api-reference/memory/get-memory),
[Get Memories](https://docs.mem0.ai/api-reference/memory/get-memories),
[Update Memory](https://docs.mem0.ai/api-reference/memory/update-memory), and
[Delete Memory](https://docs.mem0.ai/api-reference/memory/delete-memory).
The Platform's paginated Get Memories operation is partially supported by
`get_all`; see the row above for the scoped filter and pagination differences.

### Result mapping and retry behavior

Add waits for the service visibility receipt, then uses document IDs from the
durable adjudication receipt and reads each stored record back. The adapter does not invent IDs, extracted
facts, or event outcomes. If the receipt is
missing or a new document is not visible yet, add raises an uncertain-outcome
error. Supply `request_id=` and reuse it unchanged when retrying the same add;
never retry an uncertain write with a new ID. When `run_id` is omitted, an
explicit request ID deterministically supplies the request-scoped session ID
used by the service fingerprint. With no explicit request ID, Membrick
generates both the request ID and standalone session internally; that generated
identity is not durable across a caller retry.

Add accepts arbitrary JSON metadata. Each value is serialized without coercion
in the index's string-valued filter field using a reserved internal encoding
prefix, then decoded in Mem0Adapter results. Keys that collide with Membrick
ownership, retry, expiration, or supersession fields are rejected. Metadata
participates in the add fingerprint, so retrying one request ID with different
metadata fails. Update metadata replacement preserves internal fields and
ownership; omitting metadata leaves it unchanged.

Search metadata comes from stored record metadata. Internal Membrick identity
and idempotency keys are excluded. `categories`, `agent_id`, and `app_id`
are omitted because this adapter does not assign those concepts.
`created_at` is returned only when stored; `updated_at` is omitted because
Membrick does not retain a separate update timestamp. `expiration_date` is
returned only when the record has an expiration, as a UTC calendar date in
`YYYY-MM-DD` form, matching Platform response examples; otherwise it is omitted.
This differs from Platform examples that return `expiration_date: null` for
records without an expiry. Dates before `1970-01-01` are rejected because
Membrick stores expiration as unsigned Unix milliseconds.

## Filters and unsupported behavior

Search and `get_all` read `user_id` from `filters`; an explicit search
`user_id` may also be supplied if it agrees with the filter. Every search is
still restricted to that mandatory owner even when a nested `OR` expression
mentions other owners or metadata. `run_id` is accepted as an extension and
maps to Membrick's session ID.

The supported filter JSON uses nested `AND`, `OR`, and `NOT` expressions and
field conditions such as `{"priority":{"gte":3}}`,
`{"tags":{"contains":"rust"}}`, or `{"source":"*"}`. Operators are
`eq`, `ne`, `in`, `gt`, `gte`, `lt`, `lte`, and `contains`; `"*"` checks that
a field exists. `metadata` accepts an object and matches when every supplied
key/value pair equals the corresponding stored metadata value. Values remain
JSON typed: numeric comparisons are numeric, strings compare lexically, and
`contains` checks array membership (or scalar equality). `ne` matches a
missing field; other comparisons do not. Filter trees are limited to 24
nesting levels, and logical arrays must be nonempty. `icontains` is accepted
only for `user_id` and `run_id` by the typed parser, while metadata
`icontains` is rejected. Unsupported fields include `agent_id`, `app_id`,
`categories`, date/time bounds, `keywords`, and `memory_ids`; Membrick internal
identity, retry, and `membrick_*` fields are rejected as filters and omitted
from returned user metadata. Arbitrary custom fields are only considered
metadata when they were written through the Mem0 metadata API.

The adapter rejects recognized unsupported arguments with
`NotImplementedError`:

- Add: `agent_id` and `app_id`. `infer=True` is rejected because it requests
  unsupported Mem0 inference behavior; the adapter default is `False`.
- Search: `agent_id`, `app_id`, threshold, rerank, `show_expired`, categories,
  dates, keywords, and memory ID filters.
- Platform async event polling, graph memory, categories, history, exports,
  linked deletion, and hosted administration are not implemented.

Membrick performs its own semantic adjudication, which is not Mem0's fact
extraction pipeline. Do not use this adapter where application logic depends
on Mem0 extraction, category assignment, Platform REST event polling, full
Platform filters, or exact ranking behavior.

## Errors

- `ValueError`: invalid arguments, definitive service rejection, or a
  missing/foreign ID (represented locally as the documented 404 case).
- `NotImplementedError`: a recognized but unsupported Mem0 option.
- `ConnectionError`: service unavailable before dispatch.
- `RuntimeError`: dispatched operation with an uncertain outcome. Reuse the
  same explicit `request_id` to retry an add safely.

Python signature/conversion errors can raise `TypeError` or
`OverflowError` before the adapter runs. These are Membrick error categories,
not replicas of Mem0's HTTP error bodies or SDK exception classes.

`Mem0Adapter` is Python-only. Native `Memory` is available in Rust, Python,
and Node.js. Typed capture, workspace search, board, and flow operations are
Rust-only under `membrick::agent`.
