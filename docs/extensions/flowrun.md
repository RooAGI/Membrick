---
title: FlowRun extension
description: How Flowboard coordinates runs using templates, boards, and events.
---

# FlowRuns

A FlowRun records one execution of a versioned workflow and its progress.

```mermaid
flowchart TD
    A[Coordinator] -->|tools and events| F[Flowboard FlowRun]
    T[AgentFlow template catalog] -->|pinned FlowTemplate| F
    F -->|board-post capability| B[Flowboard Board/Post]
    F -->|events and projection| S[(Membrick scoped storage)]
    B -->|boards and posts| S
    F -->|action intents| H[Host or provider adapter]
    H -->|launch or wake worker and report outcome| A
```

AgentFlow publishes workflow templates. Flowboard validates the supplied
snapshot and owns FlowRun state, lifecycle rules, tools, and events. Membrick
provides scoped document storage. FlowRun uses the Board/Post capability for
board operations.

The FlowRun records workflow progress and an append-only event history. Task
details and task status live in Board/Post. Flowboard records action intents;
the host or a provider adapter handles worker dispatch and reports outcomes.

## Safe updates

Include a stable `request_id` and the expected revision when submitting an
event. Retrying the same ID and payload is idempotent; reusing an ID with a
different payload or submitting a stale revision returns a conflict.

See [Board and Post](board-post.md) and the [FlowRun HTTP API](../api/flows/index.md).
