# Temporal query resolution

Generic relative freshness is the default ranking policy for `CurrentState`
queries; other intents receive no generic freshness boost. Its implementation,
configuration, limitations, and validation results are documented in the
[freshness ranking plan](freshness-ranking-plan.md). Explicit temporal ranges
continue to use the dedicated handling described below.

Temporal language is extracted during query analysis and resolved by
`PreparedQuery` against either the query's explicit reference date or the
current date. `PreparedQuery::temporal_context()` is the shared handoff to
single-index ranking and segmented routing.

For example, “during the past month” is recognized as an explicit temporal
range. With a reference date of `2023-07-01`, it resolves to the inclusive
interval `2023-06-01` through `2023-07-01`. Month and year offsets use calendar
arithmetic; day and week offsets use day counts. The range is passed as
`starts_from` and `ends_at`, and the same bounds become the segmented routing
window. A resolved closed interval is a hard constraint: documents with
missing, unparseable, or out-of-range timestamps are removed before lexical
and hybrid candidates are collected. This prevents out-of-window candidates
from crowding the ranking before temporal scoring. Explicit ranges also suppress
the legacy point-target parser and keep sequence reranking active.

Point expressions such as “last Tuesday” continue to produce an anchor and a
seven-day routing window around it. Session-provided absolute anchors retain
their existing path.

Timestamps are normalized to UTC RFC 3339 at the indexing boundary. Legacy
Unix seconds and milliseconds are distinguished by magnitude; ambiguous small
integers such as `2026` are not interpreted as epoch values. Date-only values
remain date-only.

The existing `SourceDocument.filters` map records `timestamp_kind` so a date
retains its meaning without adding another document field. Session capture
times, explicit memory dates, source file modification times, and workspace
file-change event times are kept distinct. MCP results expose the corresponding
`created_at`, `modified_at`, or `event_at` field, and only session timestamps
are prepended to content as `[session date: ...]`.

A source file's modification time can still support explicit document-date
search and freshness ranking, but it is not treated as the effective date of a
claim or as a date phrase extracted from the document body. Session and event
timestamps likewise describe capture/event time, not necessarily when every
fact in the document became true. Temporal relations should use explicit
`valid_from` evidence when available; otherwise the stored timestamp is only a
temporal proxy. Point expressions without a closed interval keep their existing
proximity-scoring behavior; they do not activate the hard range filter.
