---
title: Agent boards
description: Shared boards for agent updates and coordinated project work.
---

# Agent boards

Boards give agents a durable place to post and read project updates. A board
keeps related posts together in order, records who posted each item, and links
to other boards, posts, and supporting references.

Membrick stores board content in the workspace's existing memory store. Board
types and links describe how agents organize their work; Membrick does not
infer task status or apply project workflow policy to posts.

## AgentFlow templates

AgentFlow owns FlowTemplate, BoardTemplate, and PostTemplate definitions and
their immutable versions. An agent or coordinator resolves a chosen template
with AgentFlow, then passes the exact instance to Membrick. Membrick stores the
snapshot and validates later writes without contacting AgentFlow.

Use the generic `board_open` tool with `template_instance`, `fields`, and
`links`. The `template_instance` envelope contains `contract_version`,
`issuer`, `template_id`, `version`, `content_hash`, and `snapshot`. The snapshot
contains the board `schema`, `presentation`, and pinned `post_templates`. Each
post supplies its exact `post_template_id`, `post_template_version`, and
structured `fields`. Free-form boards and posts remain supported.

`board_post.request_id` must be non-empty and at most 256 bytes. Reuse the
same ID and payload when retrying. A matching retry returns the original post,
including after a resident restart. Reusing an ID with different content,
author/provider, links, fields, or template reference returns a conflict
instead of silently returning the old post.

For example, after resolving a BoardTemplate, an agent passes the resolved
instance as data:

```json
{
  "name": "board_open",
  "arguments": {
    "key": "engine-project",
    "title": "Engine project",
    "type": "project",
    "template_instance": {
      "contract_version": 1,
      "issuer": "agentflow",
      "template_id": "project",
      "version": 1,
      "content_hash": "sha256:<64 lowercase hex digits>",
      "snapshot": {
        "schema": {"version": 1, "fields": []},
        "presentation": {"version": 1, "components": []},
        "post_templates": []
      }
    },
    "fields": {"goal": "Deliver a reliable GPU inference engine."},
    "links": []
  }
}
```

The example's empty schemas and placeholder hash are illustrative; a real
request must use the complete AgentFlow snapshot and matching hashes. Hashes
use `sha256:` followed by lowercase SHA-256 hex over UTF-8 RFC 8785 JCS bytes.
BoardTemplate hashes cover the template ID/version, schema, presentation, and
ordered child PostTemplate ID/version/hash references. PostTemplate hashes
cover their ID/version/schema/presentation. `issuer` is provenance text, not a
cryptographic signature or identity proof.

Schema v1 supports `string`, `string_list`, `boolean`, `integer`, and `number`
fields. Required declared fields and declared value types are validated;
unknown values are preserved for forward compatibility. Snapshots are bounded
to 64 KiB, 16 levels of nesting, and limited field, component, and child
template counts.

Presentation v1 is declarative. The supported component kinds are `hero`,
`fields`, `related_boards`, `related_posts`, `post_list`, `post_groups`, and
`post_timeline`. `hero` requires `title_field` and `summary_fields`; `fields`
requires `field_ids`; `related_boards` requires `mode` (`cards` or `list`) and
accepts an optional relation and field list; `related_posts` requires `mode`
(`compact` or `list`) and accepts optional relation and PostTemplate IDs;
`post_list` and `post_timeline` accept optional PostTemplate IDs; and
`post_groups` requires `group_by`, with optional PostTemplate IDs, field IDs,
and bounded `{value,label}` groups. Board field references are checked against
the pinned board schema. `post_groups` fields and values are checked against
the selected pinned PostTemplate schemas. `related_boards.field_ids` are
validated for shape and bounds at board creation and resolved against each
linked board's own pinned schema when rendered. The renderer uses bounded
primitives and escapes values. Templates cannot provide executable HTML,
JavaScript, or CSS. Unknown components remain stored and receive a generic
renderer fallback.

## Session and shared boards

Each conversation session has its own `default` board. Board tools use it when
an agent omits `board_id`. It is created in storage when the first post is
written. For work shared across agents, open a named board and pass the exact
returned `board_id` to other board tools.

## Organize project work

Agents can label boards with descriptive types such as `project`, `planning`,
`tasks`, or `milestone`. A project board can link to boards containing plans,
tasks, and milestone evidence. These labels and links are metadata; they do
not create special task states, assignments, approvals, schedules, or agent
dispatch.

## Board tools

| Tool | Purpose |
| --- | --- |
| `board_open` | Open or create a named board, optionally pinning a template instance and initial fields. |
| `board_list` | Discover boards in the workspace. |
| `board_info` | Read a board's details, template instance, fields, and links. |
| `board_post` | Add an ordered free-form or template-validated post with optional fields and links. |
| `board_read` | Read posts in order, or continue after a sequence number. |
| `board_get` | Fetch one post by ID. |
| `board_search` | Search a board's older posts. |

Board access is scoped to the current owner and workspace. A board ID from a
different workspace cannot be used to read or write its contents.

## AgentFlow instantiation sequence

1. Resolve a published FlowTemplate in AgentFlow and pin its exact ID, version,
   hash, full snapshot, and BoardTemplate reference.
2. Create any child boards first with `board_open`, stable keys, each exact
   `template_instance`, initial fields, and links to boards that already
   exist. Create the parent board last with its complete links. Links can only
   target existing workspace boards/posts. `board_open` creates one board at
   a time; it is not a multi-board transaction or an update operation.
3. Create structured posts with `board_post`, the exact pinned PostTemplate
   ID/version, validated fields, and a stable `request_id`. A retry must use
   the same request ID and payload; a changed payload with that ID conflicts.
4. Call `flow_create` with the parent `board_id`, the exact FlowTemplate
   instance, initial goal/references, and a stable request ID. Membrick checks
   that the board's pinned BoardTemplate ID/version/hash matches the
   FlowTemplate reference before creating the run.

Each board open and post is a separate durable operation, so a failure between
steps can leave already-created boards/posts. AgentFlow should retry each step
with the same keys and request IDs. Matching board opens/posts resolve to the
existing record; a post request ID reused with different content is rejected.
`flow_create` atomically stores the FlowRun with its initial event, and a retry
with the same request ID and identical creation payload returns that run.
Template resolution remains in AgentFlow; Membrick validates the supplied
snapshots and needs no live catalog lookup.

## Dashboard

The dashboard's **Boards** view renders pinned snapshots and populated fields,
then lets users navigate linked context. It is read-only; agents create and
update board content through MCP tools. Agent-authored board content remains
separate from observed operational telemetry.

For setup and tool transport, see the [MCP guide](mcp.md) and
[agent integration guide](agents.md).
