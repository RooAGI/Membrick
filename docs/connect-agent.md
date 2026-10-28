# Connect Membrick to your AI agent

Membrick can give supported AI agents memory that carries across work sessions.
Install it once for a project and the agent can look up earlier decisions and
save useful outcomes as it works. Memories from connected agents are kept in
the project's `.membrick/memory` folder.

Each agent uses one MCP server named `membrick`. From the project folder, use
the provider tab below to connect that project and give the agent access to its
memory. Running `membrick` with no arguments also configures user-level project
management tools for Codex, Claude Code, and AGY. Those tools can list projects
and install or remove project connections; this is the same `membrick` server,
not a second MCP server.

## Install by provider

Choose a provider to see its project setup command and provider-specific notes.

=== "Codex"

    From the project directory, install and verify the Membrick connection:

    ```bash
    curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh -s -- --agent codex --project .
    ```

    Restart Codex if it was already open. See [Codex setup](codex.md) for
    details.

=== "Claude Code"

    From the project directory, install and verify the Membrick connection:

    ```bash
    curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh -s -- --agent claude --project .
    ```

    Restart Claude Code if it was already open. See [Claude Code setup](claude-code.md)
    for details.

=== "Gemini CLI"

    From the project directory, install and verify the Membrick connection:

    ```bash
    curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh -s -- --agent gemini --project .
    ```

    Restart Gemini CLI if it was already open. See [Gemini CLI setup](gemini-cli.md)
    for details.

=== "Antigravity CLI"

    From the project directory, install and verify the Membrick connection:

    ```bash
    curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh -s -- --agent agy --project .
    ```

    Restart Antigravity CLI if it was already open. See [Antigravity CLI setup](agy.md)
    for details.

=== "OpenClaw"

    From the project directory, run `membrick install openclaw .`. Then restart
    the Gateway and check the MCP connection. See [OpenClaw setup](openclaw.md)
    for the complete steps.

=== "Hermes"

    From the project directory, run `membrick install hermes .`, then restart
    Hermes. The optional hooks plugin adds automatic recall and capture and
    requires Membrick's HTTP server to be running. See [Hermes setup](hermes.md).

=== "Muse Code"

    From the project directory, run `membrick install muse .`. See
    [Muse Code setup](muse-code.md) for installation options, verification,
    and lifecycle hooks.

=== "RooAGI AgentFlow"

    From the project directory, run `membrick install roo-runtime .`. See
    [RooAGI AgentFlow setup](roo-runtime.md) for hook and MCP details.

After setup, ask the agent about a past project decision or ask it to remember
an outcome. Membrick tools and supported lifecycle integrations work in the
background.

For example, if the `membrick` executable is installed, connect and check a
Codex project from its folder with the CLI:

```bash
membrick install codex .
membrick verify codex .
```

For other agents, use the commands in their provider tabs. `membrick setup`
remains an alias for `membrick install`. To run a lifecycle hook manually, use
`membrick hook <provider> <event> [PATH]`, for example
`membrick hook claude session-start .`. Older provider-specific flags remain
accepted for existing agent configurations and scripts.

Install options for custom paths are `--provider-config`, `--provider-settings`,
`--provider-skills-dir`, `--provider-hooks-dir`, and
`--provider-plugin-dir`. Use `--force-skill` with `install` only when you
intend to replace a customized skill.

## Uninstall by provider

Uninstall is available for Codex, Claude Code, and Antigravity CLI. From the
project directory, choose the matching command:

=== "Codex"

    ```bash
    membrick uninstall codex .
    ```

=== "Claude Code"

    ```bash
    membrick uninstall claude .
    ```

=== "Antigravity CLI"

    ```bash
    membrick uninstall agy .
    ```

Uninstall removes only Membrick or legacy Lint-AI MCP entries, hooks, and
project memory guidance for that provider. It preserves unrelated agent
settings, customized Claude skills, the project's `.membrick` memory data, and
the Membrick executable. Restart the agent after uninstalling. The Codex hook
feature flag is left unchanged because other Codex hooks may use it. Other
providers do not currently have a `membrick uninstall` command; see their
provider guide for connection-removal details.

## Migrate from Lint-AI

Existing Lint-AI users can install Membrick and update one agent connection in
one step. From the project directory, run:

```bash
curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/migrate-lint-ai.sh | sh -s -- --agent codex --project .
```

Replace `codex` with `claude`, `gemini`, or `agy` for another supported agent.
The migration installs Membrick, updates that agent's configuration while
preserving unrelated settings, and verifies the MCP connection. During agent
setup, existing `.lint-ai` project state is moved to `.membrick` with its
contents preserved. Membrick also checks for legacy state when it first accesses
the project. If both directories exist, migration merges non-conflicting entries
and moves conflicting legacy entries into `.membrick/migration-archive`, without
overwriting current state. It removes the project-root `.lint-ai` directory when
migration completes. The old `lint-ai` executable is left installed; once you have
confirmed Membrick works, remove it using the package manager that installed
it.

## What happens after setup

- The agent can search project memory when it needs earlier context.
- Supported lifecycle integrations can recall relevant memories at the start
  of work and capture outcomes when work finishes.
- You can add or remove memories yourself by editing files in
  `.membrick/memory`.
- You can turn memory behavior on or off with the agent's Membrick controls;
  this does not delete saved memories.

See [Quickstart](quickstart.md) to try memory directly from the command line,
or [Agent integrations](agents.md) for the implementation details and shared
memory behavior.

## MCP tools

The provider connection exposes these workspace tools through the agent's MCP
client. Board and flow management are documented separately in the API
reference.

| Tool | Purpose |
|---|---|
| `search` | Search indexed workspace content and return ranked results with source evidence. Results include a `search_id` for optional review feedback. |
| `info` | Show the workspace and the connection serving the request. |
| `list_memories`, `get_memory`, `add_memory`, `delete_memory` | List, read, add, or delete memories owned by the MCP integration. |
| `review_search` | Retrieve a recorded search or page through unreviewed searches. |
| `search_feedback` | Record usefulness and relevance feedback for every result from one search. |
| `enrich_doc` | Add evidence-backed searchable metadata to an indexed source document. |
| `record_session` | Start, stop, or inspect capture-only session recording. |
| `enable_membrick`, `disable_membrick`, `membrick_status` | Control and inspect memory retrieval and recording state. |
| `update_membrick` | Install an available Membrick update after the host approves it. |
| `membrick_session` | Return Codex session information; Codex only. |

The shared memory and review tools are available across provider adapters;
Codex additionally exposes `membrick_session`. Lifecycle controls vary by
provider. The MCP memory tools use a workspace-scoped `mcp` owner, separate
from memories captured automatically by lifecycle hooks. See the
[MCP memory server](mcp-memory-server.md) for examples and the
[local IPC architecture](local-ipc-architecture.md) for resident startup,
transport fallback, and leases.
