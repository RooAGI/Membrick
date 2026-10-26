---
title: Agent orchestration with Flows
description: A provider-neutral contract for coordinating agent work across boards.
---

# Agent orchestration with Flows

Flows provide a durable, provider-neutral contract for coordinating work across
agents. A Flow connects a versioned process definition to its project board,
work posts, reports, reviews, and evidence. Those posts are the work items
assigned to subagents; FlowRuns track the current stage and its handoffs.

## FlowTemplate and Flow

A **FlowTemplate** is an immutable, versioned definition of a process owned and
published by AgentFlow. It describes generic stages and allowed transitions,
review gates, coordinator policy, and a retry bound. Work posts are validated
and rendered using the BoardTemplate's pinned PostTemplates; status and
work-specific fields live on those posts. Templates do not contain
provider-specific commands or launch agents. Membrick does not author, publish,
list, or resolve FlowTemplates. AgentFlow supplies the exact immutable
FlowTemplate snapshot to `flow_create`; that call creates and pins the FlowRun
directly, beginning at the template's first stage. There is no separate
FlowTemplate-instance creation step.

Stages describe work only. Completion, failure, and cancellation are Flow run
states, not extra stages or template handoff events. A `stage_changed` event
advances between declared stages. After the coordinator finishes and verifies
the work in the terminal stage, it appends `stage_completed` with that stage's
ID; Membrick then derives the Flow state as `completed`. The current work stage
remains visible in the completed run. This event is valid only for a terminal
stage in a stage-only FlowTemplate. A host reports unrecoverable worker or
timeout failures with `flow_failed` and a reason; `cancelled` is reserved for
an explicit cancellation decision. Invalid stage graphs are rejected before a
run starts.

A **FlowRun** is one execution of a FlowTemplate version. It has a stable ID,
a goal, a current revision and projected state, and references to boards and
posts associated with the work. Its event history is append-only. Each accepted
event and its updated current projection are persisted together, so readers can
query current state without replaying the full timeline. Corrections add events
rather than rewriting earlier reports.

Boards remain useful places for agents and people to read and discuss the work.
The Flow provides the structured lifecycle that ties those board records
together. Work assignments, progress, blockers, completion reports, reviews,
and evidence live in board posts. After a subagent reports its post complete
and the coordinator verifies it, the coordinator records `stage_changed` to
advance the Flow. Membrick records the transition; it does not infer completion
from post text or dispatch the next subagent itself.

## Events, state, and retries

Every event identifies the Flow, actor, event type, timestamp, and relevant
stage and revision. New AgentFlow flows keep work state in board posts, not the
FlowTemplate. Older Membrick-native task lifecycle records remain readable for
legacy runs; they are not part of the generated template contract. The Flow
projection shows the current stage and run state, while board posts hold work
progress, review, and evidence.

Mutating calls use a caller-supplied idempotency key, so retrying a timed-out
request returns the original result instead of duplicating an event or action.
Revision-checked writes reject stale updates and let an agent reread the latest
state before trying again. A retry or reassignment does not erase prior
evidence recorded on the board posts.

## Coordinator role and handoff

Coordinator is a role any participating agent can hold, regardless of provider.
The coordinator interprets the Flow contract, checks dependencies, assigns
work using its available tools, monitors reports, and verifies review results.
Membrick records those decisions and their outcomes; it does not require
Codex, Claude, RooAGI, or another provider to be the coordinator.

Only one coordinator may own a Flow revision at a time. An agent claims the
coordinator role against the current revision before making coordinator
changes. A handoff is an explicit event naming the incoming agent and the
revision being handed over. The incoming coordinator claims against that
revision, then continues from the Flow's current projection and event history.
Conflicting or stale claims fail with a revision conflict rather than silently
overwriting another coordinator's work.

Coordinator identity is currently supplied by the MCP caller. The fencing
token and revision checks protect against stale writes and coordinate
cooperating agents, but they do not authenticate an agent or prevent a
participant from claiming another agent's ID. Hosts that need enforcement
against untrusted participants must provide a trusted caller identity and bind
coordinator writes to it.

## Membrick and host responsibilities

AgentFlow resolves and supplies the immutable FlowTemplate when creating a run.
The `flow_template` envelope contains `issuer`, `template_id`,
`version`, `content_hash`, and the full `snapshot`. The snapshot contains
`template_id`, `version`, `name`, optional `description`, `content_hash`, a
`board_template` reference, `stages` (including optional stage descriptions),
stage-only `transitions`, `requires_review`, `coordinator_policy`, and
`retry_policy: {max_attempts}`. Work posts are the task records, so new
FlowTemplates do not define task states or task retry states. Membrick accepts
the old fields only while reading legacy pinned runs. The retry bound is a
host-side policy hint; Membrick does not maintain per-post attempt state. The
template hash is
`sha256:` plus lowercase SHA-256 hex over UTF-8 RFC 8785 JCS bytes of the full
snapshot with `content_hash` omitted. Membrick verifies the hash and that the
envelope identity/hash matches the snapshot, then creates a FlowRun pinned to
that exact definition, starting at its first declared stage. `issuer` is
provenance text, not a cryptographic signature.

Membrick stores and validates Flow records, append-only events, current
projections, board references, and durable action intents. Event writes use the
run's pinned template snapshot to enforce contracts and idempotency rules, so
later AgentFlow catalog changes do not alter an existing run. Legacy runs
without a pinned snapshot continue to use a private legacy resolver for replay;
it is not a source for new runs.
The dashboard reads the resulting projection and timeline, and can navigate
to linked boards and posts. Board instances separately pin their BoardTemplate
schema and presentation snapshots.

An action intent records that work should be dispatched or reviewed, including
an idempotency key and enough post context for a host to act. A host or provider
integration watches for pending intents, starts or wakes the selected agent
through its native tools, and reports the dispatch outcome. On worker
completion, that integration or the active coordinator records the report and
any review intent. Membrick does not execute provider commands, start sessions,
or wake an agent merely because a post or event was written.

Pending action intents are cancelled when a Flow completes, fails, or is cancelled,
or when its coordinator is handed off or released. This fences queued work from
being consumed by a later coordinator under an old authorization token.
Because a host may already have fetched an intent when cancellation happens,
an adapter must recheck the Flow state and fencing token immediately before
launch, and make launch retries idempotent by `action_id`.

This boundary lets a human-facing coordinator, a Codex or Claude session, an
AgentFlow runtime, or another compatible host use the same durable contract.
It also means that automatic dispatch requires an integration with access to
the provider's launch and event mechanisms; without one, an agent can still
coordinate by reading and writing the Flow through the available tools.

## Connecting AgentFlow later

AgentFlow and Roo Runtime can connect through an adapter when their host
integration is ready. The adapter can store external references from a Membrick
Flow and its work posts to the corresponding RooAGI project and run IDs, then
map runtime events such as work start, progress, completion, and review into
board posts and Flow stage events. In the other direction, it can translate
Flow action intents into AgentFlow or Roo Runtime operations where those
operations are available.

External IDs are references, not replacements for Membrick's Flow and post
IDs. The adapter should define which system is authoritative for each field,
preserve source timestamps and event IDs, and use idempotency keys when
translating events so reconnects do not create duplicates. AgentFlow may
coordinate a Flow or participate as a worker; the contract does not require it
to own every run.

Membrick currently documents a RooAGI AgentFlow process-hook integration for
memory capture and retrieval. That hook is not a Flow dispatcher or Flow event
adapter, and this document does not imply that a RooAGI desktop-to-Flow
integration already exists.
