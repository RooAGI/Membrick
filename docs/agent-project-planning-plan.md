# Typed boards for agent-led project planning

**Status: Implementation and independent review complete.** Work remains isolated on branch `feat/agent-project-planning` in a separate worktree. This document covers the board and template layer. The generic lifecycle built on top of boards is documented in the [Flows API reference](api/flows/index.md).

## Implementation progress

- Added backward-compatible descriptive board types and generic links on boards and posts.
- Extended the shared MCP board tools with type, title, and link metadata while retaining the existing board write path and session-default behavior.
- Added workspace-scoped, read-only dashboard endpoints and a Boards view for project-first navigation, ordered posts, and linked-board/post navigation.
- Tightened dashboard pagination so `has_more` reflects whether another page exists, and expanded tool guidance for plan creation, task decomposition, assignment notes, and worker handoffs.
- Stored an opaque telemetry session key on posts when a session is known, so the dashboard can show matching lifecycle events without exposing raw session IDs.
- Added versioned built-in board templates discoverable through MCP. Each board template lists its allowed versioned post templates, and every structured post records its own post template ID and version with schema-checked fields. Task handoffs and planning posts use separate schemas; the dashboard displays both board and post template context.
- Added versioned FlowTemplate discovery and durable Flow runs with append-only lifecycle events, atomic current projections, coordinator fencing and handoff, validated task/report post links, review-gated completion, and durable action intents. Claude, Codex, Gemini CLI, and Muse expose shared Flow MCP tools; the dashboard has a read-only Flows view.
- Terminal Flows and coordinator handoff/release cancel pending action intents; polling and consumption reject terminal Flows. The Flow docs and MCP schema state that actor IDs are caller-supplied, so coordinator fences provide cooperative stale-write protection, not caller authentication.
- `cargo check --features agent-integrations --bin server`, `cargo fmt --all -- --check`, `node --check dashboard/app.js`, and `git diff --check` pass. Tests were not run. No project-specific task workflow or state machine is added.

## Objective

Let agents use Membrick's existing board feature to organize project work. An agent starts with a project goal, creates project, planning, task, and milestone boards or posts, and links related items. Other agents continue the work by reading and posting to the same linked boards. The dashboard visualizes this agent-authored structure alongside Membrick's existing operational telemetry.

Boards provide durable project content, typed posts, links, and views. Agents decide what the plan means, how to decompose it, whom to assign, and when to revise it. The Flow layer provides generic lifecycle state and coordination records across providers; it does not encode project-specific workflow rules or launch provider agents.

## Board model

Keep the existing board and ordered post model, and add a small amount of generic structure:

| Record | Proposed fields | Purpose |
| --- | --- | --- |
| Board | Existing ID, owner, workspace, key, title, creation time; add `type` and optional parent or related-board links. | A typed, addressable space for a project, planning discussion, tasks, or a milestone. |
| Board post | Existing post ID, board ID, author, provider, content, sequence, creation time; add optional title and related-item links. | An agent-authored update or project item that can point to other boards, posts, repository files, PRs, or evidence. |
| Link | Source board or post, target reference, and a short relation label. | Connects project context without imposing task-specific behavior. |

Initial board types can include `project`, `planning`, `tasks`, and `milestone`. Treat type as descriptive metadata and a dashboard presentation hint. It must not trigger type-specific workflow rules. Keep link relations generic, such as `contains`, `plans`, `tracks`, `depends_on`, or `evidence_for`; the server stores and validates the references, while the agent supplies their meaning.

A project board can contain the project goal and a summary of its stages. It links to planning boards, task boards, and milestone boards. Agents author the actual plans, task descriptions, ownership notes, progress, blockers, and results as posts, with links to related items. The board history preserves who posted each item and when.

Example structure:

```text
Project board: Chisel engine
  post: Goal and constraints
  link: planning board — M1 GPU parity
  link: tasks board — M1 implementation
  link: milestone board — M1 exit checks

Tasks board: M1 implementation
  post: Task M1.6, owner suggestion, acceptance notes
  link: milestone board — M1 exit checks
  post: Agent update with run and report links
```

The example describes content and associations. Flow events can track task lifecycle and review outcomes, but a board post alone does not dispatch an agent or establish that a milestone passed.

## Agent interaction

Use the existing board tools as the core interaction: open or discover a board, post an item, read posts in sequence, fetch a post, and search older posts. Extend these generic operations so an agent can choose a board type, link boards or posts, and discover the linked context. Board and project context should be easy to pass to child agents so workers can continue the discussion without relying on an agent manually copying an opaque ID.

Planning guidance belongs in the board tool descriptions and shared board context. For example, the tool should explain that a project board is a useful place to post a goal and link planning, task, and milestone boards. It should encourage clear, linkable agent posts while leaving planning decisions to the agent. A separate project-planning skill or provider-specific planning workflow is not part of this proposal.

### Template-guided use

Users should be able to ask a connected agent to use a template in ordinary
language, then review the result in the dashboard. MCP exposes
`board_template_list` and `board_template_get` so an agent can discover the
available contracts instead of relying on remembered field names. The agent
opens a board with a board `template_id`. The board template lists allowed,
versioned post templates. Each structured post records its own
`post_template_id` and `post_template_version` plus fields that follow that
schema. Task handoffs and planning proposals therefore have distinct schemas. The server rejects malformed posts with actionable field errors;
valid posts remain ordinary ordered board posts. Built-in board templates cover
project briefs and stages, plans and decisions, task handoffs and progress,
and milestone outcomes and reviews. Board template structure does not approve a
plan, dispatch agents, or decide whether criteria passed. FlowTemplate separately
defines generic lifecycle transitions and review gates. Custom workspace
templates can be considered separately after the built-in contract is proven.

Keep board identity, workspace ownership, post sequence, and retry behavior enforced by the existing service. Validate board types and link targets. Generic task transitions, coordinator claims/handoffs, and review gates belong to FlowRun; board types and post templates remain descriptive content contracts.

## Dashboard visualization

Add a read-only **Boards** or **Projects** view to the existing dashboard. It should:

- List project-type boards as entry points.
- Show a project board's goal and its linked planning, task, and milestone boards.
- Render each board's posts in sequence, with author and time.
- Display links between boards and posts as a navigable project map.
- Let a viewer open a linked board, read an item, and follow its repository, PR, or evidence links.
- Show agent lifecycle telemetry beside related posts when the author/session can be correlated.
- Label information by source: board post, board link, or observed telemetry.

The dashboard visualizes board content and links, and reads task/run status from the Flow projection rather than inferring it from prose. It does not assign work, launch agents, or decide that a milestone passed. If a project needs a richer display later, add it through generic board metadata or FlowTemplate contracts rather than project-specific business logic.

## Changes required

1. **Extend board records.** Add a board type and generic board/post links to the persisted board definitions and post records. Preserve existing untyped boards and session-default behavior through a migration or default type.
2. **Extend the board API and MCP tools.** Add optional type/link fields to generic board operations and a generic way to add, list, and follow links. Update tool descriptions with examples of project, planning, task, and milestone boards.
3. **Keep writes on the existing owner path.** Route board metadata and link mutations through the resident workspace owner and existing durable board writer. Avoid direct dashboard writes to index or board files.
4. **Add read-only dashboard APIs.** Return boards, their types, posts, and links for a workspace using the existing authentication and workspace scoping. Provide exact ID lookups and stable post ordering; semantic search can remain available for finding older posts.
5. **Build the board visualization.** Add a project map and linked board/post views in the dashboard. Keep telemetry as a separate observed stream and correlate it where IDs are available.
6. **Update agent setup guidance.** Add concise board-use examples to the managed provider instructions or shared board tool descriptions so connected agents know to create and link the board structure.

## Delivery stages

### Stage 1: Generic board schema

Define backward-compatible `type` and link fields, link direction and relation semantics, and how board definitions and post links are persisted. Ensure existing board posts and board IDs remain readable.

### Stage 2: Agent board tools

Extend the shared board API and MCP contracts with typed board creation and generic linking. Make linked context discoverable to parent agents and workers while retaining owner/workspace validation and ordered reads.

### Stage 3: Dashboard views

Add authenticated read APIs and a dashboard view that starts at project boards and follows links to planning, task, and milestone boards. Keep the initial dashboard read-only.

### Stage 4: Provider guidance and polish

Update setup guidance and examples, validate the workflow across connected providers, and improve telemetry correlation where provider events include matching run or agent identifiers.

## Acceptance criteria

- Agents can create typed project, planning, tasks, and milestone boards using the existing board interaction model.
- Agents can link boards and posts, and other agents can discover the linked context without manually passing every board ID.
- Posts remain ordered, attributable, workspace-scoped, durable across restarts, and safe to retry.
- Existing boards without a type continue to work.
- The dashboard can navigate from a project board through linked planning/task/milestone boards and display their posts in order.
- The dashboard distinguishes board-authored content from observed lifecycle telemetry.
- The board service remains generic. Flow adds provider-neutral task lifecycle and review gates; neither boards nor Flows directly launch provider agents or encode project-specific workflows.

## Implementation decisions

- Board types are extensible strings with a backward-compatible `general` default.
- Links are directed references from a board or post. The dashboard follows them; it does not synthesize reverse links.
- Posts support an optional title and generic links. Existing post content stays plain text.
- The dashboard uses a project-first board list and a linked board/post detail view. It correlates available lifecycle events with an opaque session key stored on the post.
- Agent guidance lives in the shared MCP board tool descriptions.
