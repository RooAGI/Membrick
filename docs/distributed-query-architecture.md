# Segmented query architecture

Membrick can query a logical corpus through one index or through multiple
segments in one resident process. The local segment router, executor, and
reducer are implemented. Remote worker fan-out is a separate future extension;
the local design does not imply a distributed or replicated service.

## Scope and terminology

The word *distributed* describes the query shape: independent segments are
queried and reduced by a coordinator. In the current deployment all components
run in one process and share one immutable snapshot.

- **Record**: the persisted unit of content and metadata.
- **Segment**: an independently searchable group of records with its own
  `MemoryIndex` and compact `SegmentProfile`.
- **Snapshot**: the immutable set of segments published for one query view.
- **Coordinator**: the query layer that routes, executes, and reduces a
  snapshot's segments.
- **Compatibility core**: a temporary single `MemoryIndex` reconstructed from
  records only for legacy dump/inspection formats. It is never part of a
  segmented snapshot and is never used for routed search.

## Invariants

The following are correctness requirements, not implementation preferences:

1. A segmented snapshot owns segments, not a redundant corpus-wide index.
2. Every segment query in one coordinator operation uses the same snapshot
   view and the same corpus-wide BM25 statistics.
3. Routing may reduce work, but it must be observable when it excludes
   segments. Explicit fallback-to-all behavior is preferable to hidden global
   index access.
4. Segment-local aggregation and coordinator aggregation are separate phases.
   Local aggregation makes each segment's candidate set useful; coordinator
   aggregation combines evidence across segments and sessions.
5. A query can return partial results only with explicit completeness
   diagnostics identifying expected, successful, and failed segments.
6. Result ordering is deterministic for equal scores.
7. Persistence format compatibility must not reintroduce retained global-index
   ownership into the runtime snapshot.

## Runtime topology

```text
                         one membrick process
                                  |
                         immutable query snapshot
                                  |
                    +-------------+-------------+
                    |                           |
             segment catalog              corpus statistics
          profiles and IDs             global BM25 provider
                    |                           |
                    +-------------+-------------+
                                  |
                         query coordinator
                   route -> execute -> reduce
                       /          |          \
                      /           |           \
                 Segment A     Segment B     Segment C
                records/index records/index records/index
                      \           |           /
                       +----------+----------+
                                  |
                  session/group aggregation -> top K
```

`SegmentedMemoryIndex` owns the segment collection, `SegmentCorpusStats`, and
the cached BM25 provider for one generation. Its generation is the publishing
`IndexStore` revision, exposed in query diagnostics so callers can correlate
results with the snapshot that produced them.
`MemoryIndexSnapshot` publishes either one ordinary index or one segmented
index; the two layouts do not share a hidden global core.

## Query lifecycle

### 1. Snapshot selection

`IndexStore` obtains the currently published snapshot. A query must use that
same snapshot for segment profiles, indexes, and statistics. Refresh publishes
a replacement snapshot with the current store revision as its generation; it
does not mutate the one already being queried.

### 2. Query preparation

The query is normalized and its routing terms are extracted. Temporal context,
reference date, allowed document IDs, and filters are prepared before routing so
the coordinator can report what was actually searched.

### 3. Routing

The coordinator scores segment profiles using `SegmentRoutingStrategy`. It
selects up to the configured segment limit and records both selected and
fallback routes. An empty or low-signal query may use an explicit fallback
policy; it must not silently consult a second corpus-wide index.

Routing is an optimization, not a scoring authority. A selected segment still
performs the real lexical/semantic query against its own index.

### 4. Statistics and execution

`GlobalBm25Statistics` combines the live Tantivy searchers from the snapshot's
segments. Each selected segment receives the same provider, so BM25 document
frequency and document-length statistics are corpus-wide even though postings
are segment-local. For a published `SegmentedMemoryIndex`, that provider is
constructed once with the snapshot and reused across queries; standalone
segment-slice helper functions construct an equivalent provider for their
short-lived call.

The execution boundary is fallible. Local execution currently implements it
directly in bounded batches of up to eight segments; a future transport may
implement the same logical operation remotely. Failures are recorded per
segment instead of being converted into an all-or-nothing query failure.
Batching bounds local thread creation while preserving deterministic
coordinator reduction.

### 5. Reduction

The coordinator merges successful segment candidates, applies the
coordinator-tier session/group aggregation, sorts deterministically, and only
then truncates to `top_k`. Segment-local aggregation remains distinct because
it serves a different purpose and operates before cross-segment evidence is
available. Each segment receives an overflow-safe candidate budget of
`2 * top_k` (with saturation at `usize::MAX`) so group collapse during reduce
does not unnecessarily starve the final result set.

The output contains results and diagnostics. Diagnostics include routing,
coverage, per-segment result counts, and completeness. Callers can therefore
distinguish “no relevant result” from “some segments did not answer.” All
segmented query variants, including enrichment and temporal-path variants,
carry the snapshot generation in their diagnostics.

## Scoring contract

Scores are comparable only within a consistent statistics view. Per-segment
normalization is not a substitute for global BM25 statistics: it can promote a
weak result from a weak segment to the same normalized score as a strong result
from another segment.

The current provider is intentionally live and local. Before remote transport,
the statistics abstraction should gain an explicit serializable form containing
query-scoped field totals, document frequencies, and a snapshot/statistics
generation. That is a transport requirement, not a reason to restore
`global_index` ownership.

## Failure and consistency model

For the single-process deployment:

- A query is evaluated against one immutable snapshot and reports its
  snapshot generation.
- Segment execution is best-effort and produces completeness diagnostics.
- A missing or failed selected segment reduces completeness; it does not imply
  that the successful results are invalid.
- Refresh creates a new snapshot and statistics view for subsequent queries.
- Normal segmented persistence stores records plus a logical `segments.json`
  manifest and rebuilds segment-local indexes without a global compatibility
  core. Older stores without a manifest fall back to deterministic group-based
  reconstruction; explicit legacy dumps may still reconstruct a compatibility
  core, but that reconstruction is outside the query path.

The architecture intentionally does not claim remote guarantees yet. Network
deadlines, worker discovery, retries, replication, and cross-process generation
negotiation belong to the transport layer.

## Implemented local behavior and evidence

The resident publishes either a single-index or segmented snapshot. Segmented
queries route to a configured set of segments, execute in bounded local batches,
and merge candidates using the same snapshot's global BM25 statistics. The
default production pipeline starts with five routed segments; adaptive
expansion and the global comparison arm are configurable. Diagnostics expose
selected and executed segments, generation, coverage, and completeness.

For retrieval quality and latency numbers, use the dated tables in the
[segmented benchmark report](benchmark.md#latest-segmented-index-comparison-v062-2026-10-16).
That report labels its release commit and configuration. Its scores are
benchmark evidence for that build, not a guarantee for every current working
tree or corpus. The older 133-query comparison is explicitly historical in
that report.

Local implementation details are covered by
[index concurrency and publication](index-concurrency.md). A current query
snapshot remains immutable while a replacement is built and published.

## Remote extension (not implemented as typed retrieval)

### Two-tier topology

The intended deployment has two distinct levels of distribution:

```text
                    lint-service coordinator
                 discovery / deadline / reduce
                       /         |         \
                      /          |          \
             membrick worker A  membrick worker B  membrick worker C
                 local                local              local
             coordinator            coordinator          coordinator
             route -> reduce        route -> reduce      route -> reduce
              / | \                  / | \                / | \
           seg seg seg             seg seg seg           seg seg seg
```

Each worker owns one corpus and may use the local segmented layout. The remote
coordinator owns worker selection and cross-corpus reduction; it must not assume
that a worker's local segment IDs or local index generation are globally unique.

### Existing `lint-service` compatibility

The existing service is useful transport infrastructure, but its current
`RunLint` protocol is a generic command-dispatch protocol:

- a request contains CLI `args`, `working_dir`, and `timeout_ms`;
- a worker launches `membrick` and returns stdout/stderr and an exit code;
- the dispatcher extracts JSON `results` from stdout and merges local `score`
  values;
- worker failures and deadlines are represented as unavailable worker results.

This supports operational fan-out and partial results, but it is not yet a
correct implementation of the typed query contract above. In particular, local
BM25 scores from different worker corpora are not guaranteed comparable, and
the response does not carry query-scoped statistics, snapshot generations,
segment completeness, or a stable result identity contract. Those are correctness
gaps, not merely optimization opportunities.

### Required remote query contract

The next transport revision should carry a versioned structured request and
response rather than scrape CLI output. At minimum:

```text
Request:  query, top_k, filters, temporal context, reference date,
          coordinator snapshot/generation, statistics contract version,
          deadline and request ID
Response: worker ID, worker snapshot generation, statistics generation,
          results with stable document IDs and score components,
          expected/successful/failed segment or worker IDs, timings
```

The remote reducer must reject or explicitly mark results whose statistics
generation is incompatible, preserve partial-result completeness, and apply
deterministic tie-breaking after merging. The worker-facing endpoint can be
gRPC or HTTP; the semantic contract is independent of that choice.

Until that protocol exists, `lint-service` is generic CLI federation rather
than a fully consistent distributed `MemoryIndex`. The structured request and
response requirements above are design guidance; no typed worker query
transport is promised here.
