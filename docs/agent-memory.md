# How Membrick remembers

Membrick gives connected AI assistants access to useful project context across
turns and sessions. It supports two complementary ways to use that memory:
automatic lifecycle hooks and explicit MCP tools.

## Automatic recall and capture

When you connect a supported agent to a project, Membrick can use its lifecycle
hooks to retrieve relevant context and capture new information as work
continues. At configured retrieval events, Membrick returns selected project
context to the agent. At supported capture events, it saves available session
content or a turn outcome and refreshes the searchable memory. A later retrieval
can then use that information; the current response is not rewritten after a
capture.

Hook events and the transcript data available vary by agent. See the
[agent lifecycle guide](agents.md#memory-lifecycle) for capture and retrieval
details, and [connect an agent](connect-agent.md) for setup instructions.

## Explicit memory actions through MCP

An agent can also use Membrick tools directly when it needs to search, save, or
manage a memory. For example, `search` finds relevant project content, while
`add_memory` saves a chosen memory. Other tools can list, read, or delete
memories and provide feedback on search results.

These are explicit tool calls. They complement lifecycle hooks, which can
retrieve context or capture content automatically. See the [MCP tool list and
examples](mcp-memory-server.md).

## Project memory

Project memory lives under `.membrick/memory/`. Connected agents use the same
project store, while Membrick keeps source and provider attribution with the
records. The resident owns project writes so connected clients share one
consistent store.

Use the MCP tools or a supported client to manage memories. You can inspect the
project data on disk, but the service should remain responsible for writing and
indexing it.

## Get started

[Connect your AI tool](connect-agent.md){ .md-button .md-button--primary }

You can also try memory from the [command line](quickstart.md) or connect an
application through the [HTTP API](api/http-overview.md), [Python](python.md),
[Node.js](node.md), or [Rust](memory-client-rust.md) client.
