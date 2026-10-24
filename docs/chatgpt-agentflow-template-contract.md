# ChatGPT AgentFlow template contract

Status: product and permission contract for the personal-workspace first
release. It aligns the AgentFlow catalog resolver with Membrick's canonical
FlowRun and board APIs. It does not add an MCP server or change tool handlers.

## User journey and ownership

1. A user browses and previews immutable templates on the AgentFlow website.
2. **Add to ChatGPT** installs one AgentFlow skill and its MCP connection. It
   selects a template but creates no FlowRun, board, or post.
3. In ChatGPT, the user selects an exact template version, reviews the
   resolved board/post bundle and goal, and approves starting it.
4. AgentFlow exposes the workflow tools and coordinates the run. Membrick
   stores the canonical FlowRun record, boards, and posts; its dashboard reads
   those same records. A provider runtime owns its own execution/checkpoint
   state and correlates it to the FlowRun/task IDs.

AgentFlow owns template authoring, publishing, immutable versions, and
resolution. Its catalog resolver returns the exact `flow_template_instance`
and board `template_instance` snapshots. Membrick verifies and pins those
snapshots; it does not query AgentFlow to render or validate an existing
instance. Each `content_hash` is `sha256:` plus lowercase SHA-256 hex of the
RFC 8785 canonical JSON snapshot with `content_hash` omitted. This checks
integrity, not publisher identity. AgentFlow must not keep a second board/post
database.

## ChatGPT-facing tools

The first gateway exposes only these tools. The names below are the public
single-server names; an aggregate host may prefix them to avoid collisions.

| Tool | Access | Responsibility |
| --- | --- | --- |
| `template_search(query, limit?)` | Read | Search published templates visible to this user's configured AgentFlow catalog. |
| `template_get(template_id, version?)` | Read | Return one immutable definition and its `content_hash`. Omitting version may select latest for discovery only. |
| `template_preview(template_id, version)` | Read | Resolve that exact FlowTemplate and return its child board/post snapshots, proposed board composition, required inputs, references, expected provider/runtime, and hash. It performs no writes. |
| `flow_start(request_id, template_id, template_version, expected_content_hash, goal, fields?, references?)` | Write | Re-resolve the exact version, require the hash to match the preview, create the board instances and FlowRun through Membrick, then return their IDs and pinned refs. It does not resolve `latest` on retry. |
| `flow_status(flow_run_id)` | Read | Read the authorized FlowRun projection and linked board/post IDs from Membrick. |
| `board_read(flow_run_id, board_id, after_sequence?, limit?)` | Read | Read an exact board only when it is linked to this authorized FlowRun. |
| `board_post(request_id, flow_run_id, board_id, title?, content?, post_template_id?, post_template_version?, fields?, links?)` | Write | Create one post on a board linked to the authorized FlowRun, using its pinned PostTemplate when present. |

Do not expose raw `board_open`, workspace selection, arbitrary Flow event
append, or cross-workspace board enumeration to this ChatGPT integration.
`flow_start` is the only initial path that creates boards. Cancellation and
retry are out of the initial tool set; if added later, each must be a separate
revision-checked, approval-gated mutation.

At the backend boundary, AgentFlow resolves the exact catalog bundle, calls
Membrick's generic `board_open` with the pinned BoardTemplate instance, then
calls Membrick `flow_create` with the pinned FlowTemplate instance. Status
maps to `flow_get`; board operations map to `board_read` and `board_post` on
the same resident records. These internal Membrick tools are not separately
exposed to ChatGPT.

## Identity and workspace binding

ChatGPT connects through Secure MCP Tunnel to a dedicated AgentFlow gateway
bound to loopback. The gateway reaches the local Membrick resident over its
private IPC. Do not tunnel the resident HTTP listener or dashboard, and do
not enable a public listener as a fallback.

Each gateway installation is bound by trusted installer-managed configuration
to one local Membrick owner and workspace, one resident IPC endpoint, and the
configured AgentFlow catalog. Tool arguments may contain opaque FlowRun,
board, and post IDs; they must never choose an owner, workspace path, socket,
Membrick token, provider identity, or arbitrary backend URL. The gateway
injects the bound owner/workspace and a fixed allowlisted Membrick provider
identity on every call. It checks that a supplied board is linked to the
FlowRun and that Membrick validates every record under that same scope.

The tunnel provides reachability; it is not by itself proof of a user's
identity or write permission. Before enabling writes, the host must establish
an authenticated ChatGPT connection or a private single-user pairing for the
gateway and bind it to the local installation. If the supported tunnel cannot
preserve that binding, the gateway must refuse writes rather than accept an
unattributed caller. `issuer` in a template envelope is provenance text, not
authentication; templates must come from the configured trusted AgentFlow
catalog, and the hash check provides integrity rather than publisher identity.

## Confirmation and run-scoped authority

Installing the skill, searching, getting, previewing, reading status, and
reading boards are read-only. They must not create a run or board.

Before `flow_start`, ChatGPT must present the selected template ID, exact
version and hash, goal, configured workspace label, boards/posts to be
created, provided fields, referenced existing records, and the provider/runtime
capabilities and external effects AgentFlow expects. A template cannot grant
itself access beyond the user's existing provider and tool permissions. If
the host cannot resolve the requested execution capabilities, it must surface
that gap and not start the run. The supported
ChatGPT approval UI must approve the mutating tool call. If that UI is not
available, the integration must obtain explicit confirmation in its own
trusted UI before calling the tool. A model-supplied `confirmed: true` value
is never evidence of approval. The gateway requires `expected_content_hash`
and verifies it again immediately before writing.

Approval of `flow_start` authorizes only that run: creating the previewed
boards and FlowRun, and bounded lifecycle/progress/review posts on those
boards using the pinned templates. AgentFlow must not use that approval to
write unrelated boards, start another run, change workspace scope, or invent
new template versions. A direct ChatGPT `board_post` is a separate mutation
and requires its own approval. All writes carry stable request IDs and audit
the authenticated principal, run/board IDs, tool, and outcome without logging
secrets or full post content by default.

## Safe failure behavior

| Failure | Required behavior |
| --- | --- |
| Missing, invalid, or revoked ChatGPT/gateway authorization | Fail closed before the catalog or resident mutation call. |
| Template version or expected hash is missing, stale, untrusted, or invalid | Return a conflict/validation error; create no board or run; require a fresh preview and approval. |
| Workspace mismatch, unlinked board, or unauthorized reference | Reject without revealing whether an out-of-scope record exists. Never try another workspace. |
| Invalid fields, unsupported snapshot, or PostTemplate mismatch | Reject before committing the affected record; preserve the pinned definition and any earlier valid records. |
| Same request ID and same payload | Return the original result. Same request ID with different content is a conflict. |
| Timeout after a write may have reached Membrick | Report outcome as unknown and retry/reconcile with the same request ID. Never mint a replacement ID automatically. |
| Partial multi-board start or resident unavailable | Return the failed stage and known IDs; retain idempotent creation keys so retry can resume without duplicates. Do not silently delete already-created boards or fall back to a public endpoint. |
| Agent/runtime failure | Keep prior events and posts; show failed/blocked status and a safe retry path. Do not report success or erase evidence. |
| Confirmation UI or tunnel identity binding unavailable | Disable the write tool. Read operations may continue only if the authenticated workspace binding remains valid. |

The local MCP gateway package is still an integration boundary: the current
AgentFlow work defines catalog resolution and backend interfaces, while
ChatGPT tunnel/OAuth handoff remains a host deployment dependency. Do not
claim that the connection is production-ready until the authenticated tunnel
binding and mutation approval path are verified end to end.
