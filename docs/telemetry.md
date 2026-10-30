# Telemetry

## Installer telemetry

The shell and PowerShell installers report after a successful installation.
Cargo, PyPI, and npm installs report on the first interactive CLI use because
those package managers do not provide a reliable post-install hook. Without an
explicit sharing flag, interactive runs ask first with No as the default, and
unattended runs send nothing. Passing `--share-telemetry` (shell) or
`-ShareTelemetry` (PowerShell) opts in directly. Reporting is best-effort and
cannot fail installation or CLI use.

With consent, the installer sends the Membrick version, operating system, CPU
architecture, installer type (`install_sh`, `install_ps1`, `cargo`,
`cargo_build`, `pip`, or `npm`), selected agent, install result, and timestamp.
Release binaries built in a Cargo `target/release` directory are reported as
`cargo_build`; debug builds do not prompt. It creates a random
install ID in the user's config directory to count repeat install events
without an account. The payload excludes names, email addresses, project paths,
and memory data. The service necessarily sees the request's network address;
configure the service and proxy logs to avoid retaining it if that is not
needed. Cargo/PyPI/npm installs that are never run interactively cannot be
counted by this client.

The receiver is `POST /v1/telemetry/install` on `license.rooagi.com`. The
`license_server` service needs its `membrick_install_events` PostgreSQL table,
created by `migrations/20261030_membrick_install_events.sql`, plus the
`migrations/20261030_membrick_installers.sql` constraint update covering Cargo,
Cargo release builds, pip, and npm sources. Deploy the updated Axum service with
both migrations applied.
`queries/membrick-install-telemetry.sql` contains starter reports for install
runs and distinct successful installs. Those reports count pseudonymous install
IDs, not verified people or devices.

Membrick telemetry is local operational data used to understand query behavior
and provider activity. It is separate from indexed memory and session archives.
Aggregate telemetry is bounded. Each detailed MCP tool history entry is capped
at 16 MiB; oversized arguments or responses are recorded as omission markers.

## Telemetry streams

Membrick currently has four related streams:

| Stream | What it records | Storage |
| --- | --- | --- |
| HTTP query telemetry | Request count, errors, empty results, and latency distribution | In-memory in the running HTTP server |
| MCP query telemetry | The same project-level query aggregates for provider MCP processes | `.membrick/query-telemetry.json` |
| MCP tool history | Detailed MCP tool inputs and the response returned to the provider | `.membrick/mcp-call-history.jsonl` |
| Provider lifecycle telemetry | Provider hook events and available token usage | `.membrick/provider-telemetry/{provider}.json` |

HTTP query telemetry is process-local. Provider MCP processes persist their
query aggregates so the dashboard can observe queries handled outside the HTTP
server process. The query aggregates are project-wide and do not attribute a
request to a provider. Detailed history is recorded after every provider
`tools/call`, including search, memory reads and writes, memory controls, and
update actions. The Claude, Codex, Gemini CLI, AGY, Muse, OpenClaw, Hermes, and
Roo runtime adapters all use the same recorder.

## Query aggregates

Queries are grouped into five-second buckets. The most recent 120 buckets are
kept, giving a rolling ten-minute window. Each bucket tracks request count,
error count, empty-result count, and a latency histogram. The dashboard derives
request rate and latency summaries from these aggregates. Query strings,
session IDs, and document contents are not stored in this aggregate stream.

The detailed MCP tool history is an append-only JSON Lines file with no
retention or entry-count limit. Each entry includes provider, tool name,
arguments, timestamp, elapsed time, success or error status, a stable
`trace_id`, and the response returned by the resident handler. An entry is
capped at 16 MiB; when its serialized arguments and response exceed that limit,
both are replaced with omission markers while the trace ID and call metadata
are preserved. `session_attribution` is `explicit` when the caller supplied a
session ID, `inferred` when the resident resolved the provider's active session,
or `absent` when no session applies. The resident's internal attribution
metadata is removed before the provider receives the search response. Review
can include session documents for explicit and resident-inferred sessions;
absent attribution never selects a session by timestamp proximity.
Session IDs and content are retained as supplied. New fields are optional when
reading older entries. On Unix, the file is created with
owner-only permissions. Entries can contain private project information and
will grow until manually removed, so treat this file as sensitive and account
for its disk usage. It is an operational archive, not a semantic memory
document, and is not included in normal memory search results.

The MCP tools use this history for search review. An ordinary search returns a
stable ID, which the agent can pass directly to `search_feedback` when results
are inadequate. `review_search` is a separate tool for retrieving one exact
search or paging through unreviewed searches; it is not required before direct
feedback. Search guidance prompts the agent conditionally and does not submit
feedback automatically. `search_feedback` attaches an overall label and
explanation to one search, plus a relevance judgment for every returned
document. The service preserves each document's original rank and source hash
from the recorded result. These judgments support evaluation; they do not
change ranking or mutate documents. Feedback may also add grounded terms to
supported query-only rules. `enrich_doc` is the separate tool for an
evidence-backed update to one exact source document ID and revision, such as
adding a session conclusion, headings, or links to reviewed observations.
Later hook captures preserve those enrichments while retaining raw
observations as separate documents. These tools do not rewrite the append-only
history. Queue pagination streams JSONL records through a bounded buffer and
retains only the current response page. The opaque continuation cursor
includes a byte offset and a fingerprint of the history prefix already read.
Cursor validation hashes that prefix with a fixed-size buffer; if the prefix is
replaced or edited, the cursor is rejected and pagination should restart from
the first page. Appending new records does not invalidate an existing cursor.
Each page is capped at 8 MiB of history records, and records over 16 MiB are
skipped and counted in the response.
See the [MCP tools](connect-agent.md#mcp-tools) for the supported operations.

## Provider lifecycle events

Provider events are kept in a separate JSON ledger for each provider. Each
ledger retains at most the latest 500 events. The event schema can include:

- event name, category, and timestamp
- a one-way session key rather than the provider's raw session ID
- bounded agent, turn, and tool metadata when the provider supplies it
- redacted, bounded previews for selected prompt or tool activity
- retrieval timing and result counts when available
- normalized token counts when available

Lifecycle telemetry is written independently of session recording. Turning off
session recording prevents full event archives from being written; it does not
by itself disable operational lifecycle telemetry. Full session content is not
copied wholesale into the telemetry ledger.

## Provider coverage

Coverage depends on what each provider exposes and which integration features
are compiled:

| Provider | Query aggregates | Lifecycle events | Token usage |
| --- | --- | --- | --- |
| Claude Code | Yes | Yes | Yes, when hook or transcript data includes usage |
| Codex | Yes | Yes | Yes, when hook or transcript data includes usage |
| Gemini CLI | Yes | Yes | Supported when usage data is available |
| AGY | Yes | Yes | Supported when usage data is available |
| Muse Code | Yes | Yes | Not currently wired to the shared transcript usage path |
| OpenClaw | Yes | Yes | Not currently wired to the shared transcript usage path |
| Hermes | Yes | No Hermes lifecycle events are currently wired into the provider telemetry ledger | Not currently wired to the shared transcript usage path |

“Yes” means the code path is instrumented; it does not mean telemetry exists
for a provider in a particular workspace. A provider remains `not_observed`
until it emits an event. Token usage may be unavailable when the host does not
include usage data in its hook payload or transcript.

The repository also contains a separate optional Python Hermes hooks plugin
for memory capture. That plugin is not connected to the provider telemetry
ledger described here, so it does not make Hermes lifecycle telemetry appear
on the dashboard.

## Dashboard and API

The local dashboard reads query and provider telemetry through these endpoints:

| Endpoint | Purpose |
| --- | --- |
| `GET /api/status` | Index state, query summary, and integration cards |
| `GET /api/timeseries` | Five-second query aggregates over the rolling window |
| `GET /api/search-reviews?offset=0&page_size=25` | Paginated rows from the latest offline search-feedback snapshot |
| `GET /api/integrations` | Provider readiness |
| `GET /api/sessions` | Recent sessions and sanitized lifecycle counts |
| `GET /api/events` | Latest sanitized provider events |
| `GET /api/sessions/:session_key/events` | Events for one opaque session key |
| `GET /api/metrics` and `GET /metrics` | Aggregate metrics, including Prometheus text format |

The dashboard's **Search reviews** view reads one page-ready snapshot at
`.membrick/search-feedback-dashboard.jsonl`. Its first JSONL row contains the
evaluation summary and snapshot metadata; later rows contain the paginated
reviews. It does not read the MCP history or review state, or acquire the
resident memory lock. Refresh the snapshot by running the separate evaluator
when desired:

```bash
cargo run --features experimental --bin feedback_eval -- \
  --history .membrick/mcp-call-history.jsonl \
  --reviews .membrick/search-review-state.json
```

The evaluator runs as a separate process and writes derived dashboard files;
MCP search and feedback calls never invoke it. Metrics evaluate recorded
ranking only; feedback does not train or alter the live ranker. Dashboard pages
are bounded and use explicit feedback labels rather than treating unreviewed
searches as negative examples. The dashboard reports when its snapshot was last
generated and uses the same workspace telemetry authentication rules as the
other workspace-wide views.

Run the dashboard renderer unit tests with `node --test dashboard/search-evaluation.test.mjs`.

See [HTTP server](server.md) for starting the dashboard and configuring its
project root and authentication.

## File watcher changes

Workspace file diffs are not operational telemetry. The watcher captures
bounded before-and-after snapshots and persists eligible diffs as memory
`SourceDocument`s. It does not send file contents or diffs to the telemetry
ledgers. This keeps source content in the memory store and operational metrics
in the telemetry path.

Content capture skips `.env` files, private-key files, and standard credential
locations such as `.ssh` and `.aws`. Known credential values and sensitive
assignments are redacted from captured diff lines. This is best-effort filtering;
custom secret formats and arbitrary secret filenames may need additional ignore
rules. Existing stored documents are not retroactively sanitized.

On Unix, snapshot reads reject symlinks in every path component and use held
directory descriptors to avoid check/open races. Reads are bounded to 256 KiB.
Automatic snapshots and `rooagi_runtime` file payload references are unavailable
on non-Unix targets until an equivalent safe reader is implemented. Watcher
invalidation and inline hook payloads continue to work there.

## Implementation references

- `src/telemetry.rs` implements query aggregates, provider event ledgers, and
  MCP call history.
- `src/integrations/session_recording.rs` normalizes provider events and token
  usage, and separates operational telemetry from full session recording.
- `src/pipeline/watcher.rs` captures bounded workspace file snapshots for
  diffs.
