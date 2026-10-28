# Index concurrency and publication

Membrick separates one mutable writer from immutable search snapshots. The
resident owns a project's persistent stores and is the only process allowed to
write them. Search requests keep using a published snapshot while accepted
changes are applied and a replacement snapshot is built.

## Ownership and workspace isolation

The resident has one workspace runtime per open project root. It canonicalizes
requested roots, checks them against the configured allowlist, and initializes
allowed additional workspaces on first use. The default active-workspace cap is
8, including the primary workspace; `--max-active-workspaces` changes it. An
allowed but unopened workspace is rejected when the cap is full rather than
opened implicitly.

Each runtime coordinates two logical stores: project source documents and
captured agent memories. They share the resident's bounded writer queue,
publication worker, and composed search view, while `MemoryService` selects the
target store for each operation. Search reads use the matching immutable view.
The resident's IPC and HTTP listeners route into these runtimes. A standalone
server or an embedded `MemoryService` must acquire the project owner lock before
opening a project's store, so two processes cannot write the same project
concurrently.

See [local IPC architecture](local-ipc-architecture.md) for startup and
transport details and the [`MemoryService` boundary](memory-service-api.md) for
the domain API.

## Staged writes through HTTP or IPC

For disk-backed workspace writes through the staged HTTP and IPC endpoints,
the resident validates the operation, appends and syncs its transaction to
`pending-adds.jsonl`, and applies it to the mutable store. The acknowledgement
may say `published: false`: the write is durable and accepted, but a search
snapshot containing it is not published yet.

Publication is scheduled 250 ms after the first pending add or when 512
documents are pending. These are scheduling triggers, not visibility
deadlines. The writer continues accepting work while one background builder
constructs the next snapshot. Admission is bounded: the command queue holds 32
commands and unpublished accumulation is capped at 32,768 documents. Saturated
requests receive `429` before acceptance.

Full checkpoints run every 30 seconds and on explicit flush. A checkpoint
captures a journal boundary and a matching record/index generation, persists
that state, then reclaims only journal entries covered by the checkpoint.
Startup replays complete transaction lines idempotently and discards an
interrupted final line. Corruption in a committed transaction fails startup
rather than silently losing accepted data.

`POST /flush` and `POST /v1/memories/refresh` wait for the captured target
revision to publish and checkpoint. `wait_for_visibility=true` on an add waits
for the accepted write to reach that published state and returns its final
receipt. Later writes do not move an earlier request's target. A request
cancellation does not undo work already admitted to the writer. Shutdown drains
requests and flushes; after a crash, the journal is replayed.

Memory operations exposed by `MemoryService` and many provider endpoints
publish synchronously. Source-document upserts and durable transcript capture
can use the resident's staged publication and capture workers. The caller's
operation contract determines whether acceptance, persistence, or published
visibility is the completion point. In-memory services can publish snapshots
but do not provide disk-restart durability.

## Single and segmented snapshots

`MemoryIndexLayout` selects a single index, a fixed-width segmented index, or
adaptive segmented routing. A snapshot is replaced as one generation; readers
never observe in-place mutation of its postings. Segmented refreshes can reuse
unchanged segment indexes and rebuild affected segments. Single-index refreshes
rebuild the single index. Routing diagnostics identify selected and executed
segments, and segmented queries use shared corpus BM25 statistics when merging
their candidates.

The resident's production pipeline defaults to a segmented layout with a
five-segment starting breadth. Adaptive expansion, global-arm fusion, and
other query settings are configurable. See [distributed query architecture](distributed-query-architecture.md)
for the local segment query and reduction model. The word “distributed” there
describes fan-out across local segments; it does not mean that the resident is
a replicated multi-node writer.

## Operational guidance

- Route project writes through its resident. Do not open a second writable
  service for the same store.
- Use the staged HTTP endpoint when asynchronous publication is useful; set
  `wait_for_visibility=true` when the caller needs a searchable result before
  returning.
- Treat `429` admission errors as not accepted and retry after backoff with the
  same request identity when the operation contract supports it.
- Monitor queue admission, pending documents, publication latency, checkpoint
  duration, snapshot generation, selected segments, and completeness.

For server flags and request behavior, see the [server guide](server.md) and
[HTTP API reference](api/http-overview.md).
