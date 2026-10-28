# Search review lookup design

Status: core locator implementation complete. The resident rebuilds an
in-memory search locator in bounded batches and routes exact lookup, feedback
lookup, and pagination through it. Recorder crash recovery and operational
status reporting remain separate follow-up work.

## Decision

Use a resident-owned `std::collections::HashMap` that maps existing search IDs
to byte offsets and lengths in `.membrick/mcp-call-history.jsonl`. Rebuild the
map from telemetry after every resident restart. Add no database, persistent
locator files, or second copy of recorded queries and results.

The telemetry history remains the authoritative search record. Losing the
in-memory map loses lookup acceleration only. Rebuilding costs startup I/O and
CPU; keeping all search locations costs memory proportional to search count.
Measure those costs before considering persistent indexing.

This complements the [telemetry architecture](telemetry.md). `MemoryService` owns operational access.
The offline dashboard remains a derived consumer of saved history and feedback.

## Validation of the telemetry assumption

The current recorder in `src/telemetry.rs` writes complete MCP call arguments
and provider-facing responses as newline-terminated JSON records in append mode.
There is no recorder retention or payload-size limit. The shared MCP response
wrapper inserts a search ID into successful search responses and records the
same ID as `trace_id`. It operates independently of optional session recording.
The existing Rust recorder tests and response-ID consistency test pass.

A bounded inspection of the project history found:

| Check | Observed result |
|---|---|
| File size at the inspected boundary | 412,593,594 bytes |
| Search records parsed | 173 |
| Modern search records with trace IDs | 66 |
| Successful modern searches with matching IDs and result arrays | 65 |
| Modern searches containing a recorded error | 1 |
| Legacy searches without trace IDs | 107 |
| Duplicate modern search IDs observed | 0 |
| Modern locations rebuilt and verified after reopening the file | 66 of 66 |

These are observations of one fixed local history boundary, not evidence that
all calls were recorded. The bounded reader also identified a 407,163,384-byte
`review_search` row from the older implementation, using its header and byte
boundaries without parsing the entire payload. It is unrelated to search-map
entries and must be skipped without allocating its complete contents.

Older searches remain usable by reusing the existing `mcp_search_id()` legacy
ID derivation. Do not substitute a different raw-line hashing algorithm.

### Reliability limits to resolve

The history is sufficient to reconstruct searches that were successfully
recorded. The existing recorder does not guarantee every successful search has
such a record:

- `record_mcp_tool_response()` prints an append error and still returns the
  provider response. A returned search ID can therefore have no saved record.
- The telemetry append lock is a create-new marker removed on normal drop.
  A process crash can leave it behind, causing subsequent recording attempts
  to time out. Use project ownership and an OS lock for crash-safe coordination;
  do not blindly remove a marker that could belong to a live recorder.
- `write_all()` is used without an explicit disk sync. Ordinary process restart
  does not itself erase completed writes, but power-loss durability of the last
  records is not guaranteed. Choose and document the acknowledgement and sync
  policy; adding a second copy of search data would not solve this contract.
- Recording occurs in provider MCP response wrappers. Direct resident calls,
  automatic hook retrieval, and generic HTTP searches do not all pass through
  those wrappers. This design initially indexes recorded MCP tool searches.

Surface recording health and distinguish pending indexing, a recorded failed
search, and an ID absent after catch-up. Never claim the index can reconstruct
a request whose history append failed. Expanding capture coverage or changing
search failure behavior requires an explicit contract and validation.

## Reuse existing Rust components

| Need | Component |
|---|---|
| Search ID to location in memory | `std::collections::HashMap` |
| Read one saved record | `std::fs::File`, `std::io::Seek`, bounded `Read` |
| Stream history | Existing bounded JSONL reader and `BufReader` |
| Parse existing records | Existing `McpCallRecord`, `serde`, and `serde_json` |
| Identify modern and legacy searches | Existing `mcp_search_id()` logic |
| Synchronize map publication | Existing resident ownership and Rust synchronization |
| Schedule rebuild and catch-up | Existing resident background worker pattern |

Rust's standard library provides the collection and file APIs. Its `core`
crate itself provides no filesystem storage. See the official
[HashMap](https://doc.rust-lang.org/std/collections/struct.HashMap.html),
[Seek](https://doc.rust-lang.org/std/io/trait.Seek.html), and
[File](https://doc.rust-lang.org/std/fs/struct.File.html) documentation.

## Map contents and ownership

Conceptually, store:

```text
HashMap<SearchId, (ByteOffset, ByteLength)>
```

Offsets and lengths use checked `u64` byte counts and include the record's final
newline. Maintain the history identity, indexed byte position, and rebuild
status in memory alongside the map. Store no complete query or response bodies
in the map. Reuse existing search ID strings and MCP record types.

One project resident owns the map through `MemoryService`. Concurrent lookups
hold a short read lock only long enough to copy a location. Parsing, history
I/O, and response serialization run outside that lock. Each read uses an
independent file handle so callers never share a mutable seek position.

Recorder adapters can run in separate processes. The design must not assume
they share the resident map. The resident tails completed history records,
with bounded periodic catch-up and optional wake notifications. History bytes
are the recovery source if a notification is missed.

## Startup, lookup, and append flow

1. Workspace resident startup creates an empty map and schedules background
   reconstruction from byte zero. Normal memory search does not wait for a
   complete scan.
2. The worker reads at most 1 MiB per batch, polls every 100 ms, and publishes
   locations for completed search records. It advances its in-memory position
   only past consumed bytes and retains incomplete or oversized row state.
3. Exact review copies a location, checks history identity and arithmetic, seeks
   to the saved offset, and reads that record. Verify termination, record type,
   and search ID before returning it.
4. If an ID is not indexed while reconstruction is incomplete, return indexing
   pending with progress and schedule catch-up. Do not scan the full file inside
   the request or incorrectly report not found.
5. After catch-up to a captured history end, an absent ID can be reported as
   absent from recorded history. A later append can make it available.
6. Appends extend the existing map. Resident restart discards the map and repeats
   reconstruction without creating any persistent locator or progress file.

Expose indexed bytes, captured end, entries, and pending/error state through
existing operational status mechanisms. Do not invent public option or
configuration families for this internal implementation.

## Recovery and bounded work

File identity changes or truncation invalidate the map. Schedule a rebuild and
reject stale locations. Verify the ID at a location to catch selected-record
corruption or a replacement race. Never return an arbitrary record just because
an offset remains inside the current file. Full detection of arbitrary edits to
unvisited history is a separate offline verification task.

Consume only newline-terminated records. Retain an incomplete trailing record
for later catch-up. A crashed partial append needs explicit recovery under the
append lock before new bytes can merge into it. Preserve original bytes when
reporting or isolating corruption.

Limit bytes and records processed per worker batch, including malformed,
non-search, and oversized data. Keep partial skip state in memory so a huge
unrelated row can be drained over several batches. Reuse bounded record parsing
and report search records that exceed the supported parsing limit rather than
silently claiming full indexing coverage. Do not change recorder retention.

Detect conflicting duplicate search IDs instead of overwriting their locations
silently. Test collisions and preserve enough state to report ambiguity.

## Review pagination and response bounds

A map solves exact lookup; it does not by itself solve unreviewed pagination.
Use a byte continuation cursor over a fixed history end boundary, with separate
limits for scanned bytes, records examined, and complete serialized output.
Return a progressing cursor when scan work ends, including pages with no matches.
Count reviewed, malformed, oversized, and unrelated rows against scan work.

Replace whole-prefix cursor hashing with history identity plus bounded boundary
validation. When skipping a huge row, preserve explicit continuation state;
never parse its middle as a new JSON record. Keep active traversal progress in
memory; a resident restart may require the caller to restart pagination.

Budget source document metadata and duplicated result payloads as well as
content previews. Truncation and original sizes must be explicit. Oversized
items need a bounded summary or an actionable error that allows traversal to
advance. Keep full source documents and original history unchanged.

Validate query-rule grounding against full authoritative source documents,
independently of the display preview. Avoid concatenating all documents into
one unbounded JSON string during that validation.

## Feedback and dashboard scope

Keep existing feedback persistence while implementing the search lookup map.
Feedback is new information that must still be saved; the search map does not
replace it. Loading and rewriting the entire feedback state remains a separate
scaling problem. An incremental in-memory review lookup can be reconstructed
from existing saved feedback, but changes to its persistence format or
acknowledgement protocol need a separate design decision.

The offline evaluator continues to produce the dashboard snapshot. Dashboard
requests never trigger history reconstruction, aggregation, or feedback
migration. A map rebuild must not acquire the mutable memory writer for its
entire scan or delay normal search publication.

## Implementation and validation

1. Harden and document the existing recording contract, especially stale-lock
   recovery, append failures, and partial tails.
2. Add the resident-owned map, background reconstruction, and incremental
   catch-up through `MemoryService`.
3. Route exact review and feedback search lookup through checked map locations.
4. Route pagination through bounded windows of indexed locations and cap page
   and exact-search responses at 8 MiB. Separate rule grounding from previews.
5. Measure memory growth and rebuild cost before considering further storage work.

| Scenario | Required proof |
|---|---|
| Map rebuilt after resident restart | Same modern and legacy IDs resolve to the same saved records |
| Exact lookups in increasingly large histories | Read bytes depend on selected record size rather than total history |
| Rebuild concurrent with appends | No missed complete records and no shared seek races |
| Missing ID during rebuild | Pending response, with eventual lookup or accurate absence |
| Multi-process recording | Resident catches up without relying on shared adapter memory |
| Truncated, replaced, or corrupt history | Stale map entries are rejected and recovery progresses |
| Huge unrelated rows or partial tails | Worker budgets hold and normal searches remain responsive |
| Recorder crash or failed append | Lock recovers safely; recording failure is observable |
| Duplicate IDs | Ambiguity is reported instead of returning the wrong search |
| Large metadata and UTF-8/JSON escaping | Complete response stays inside the output budget |
| Grounding term beyond a display preview | Valid full-source evidence is accepted |

## Implemented behavior

`MemoryService` owns a per-project in-memory locator. The exact review and
`search_feedback` paths seek directly to the indexed JSONL record and verify
the history generation, record boundary, tool name, and search ID. Pagination
copies no more than 501 indexed locations at a time, examines at most 500 per
call, preserves a fixed snapshot end in the cursor, and returns `status:
indexing` while the worker is behind or has an I/O error. Page history reads
and serialized responses are both capped at 8 MiB; a single exact response
over that cap returns an actionable error. Nothing persists the locator.

The in-memory locator is shared across published and segmented `MemoryService`
views. The worker starts as each resident workspace owner opens. Review
feedback remains in its existing state file; the telemetry JSONL remains the
only durable copy of search calls. The resident also rebuilds a per-project
`search_id → feedback entry` map from the feedback state file once, shares it
with published read views, and updates it when feedback is saved. Page checks
use this map instead of rereading the full state file. The durable JSON
snapshot is still atomically rewritten on feedback updates, so write
amplification remains linear in the number of saved reviews and can be
revisited separately.

Measure allocation growth per map entry, peak rebuild memory, bytes scanned,
startup catch-up duration, and concurrent search/write latency percentiles.
Use regression tests that fail on the relevant existing behavior before fixes.
The live inspection and existing recorder tests validate data availability;
they do not yet prove a production Rust reconstruction worker or crash recovery.
