---
meta:
  - property: og:type
    content: website
  - property: og:title
    content: "MCP Memory Server: Agent Memory Over the Model Context Protocol"
  - property: og:description
    content: "Membrick exposes persistent agent memory as an MCP server: search, session recording, and memory controls as tools any MCP-capable client can call."
  - property: og:image
    content: https://rooagi.github.io/Membrick/assets/images/social/mcp-memory-server-membrick.png
  - property: og:image:width
    content: "1200"
  - property: og:image:height
    content: "630"
  - property: og:url
    content: https://rooagi.github.io/Membrick/mcp-memory-server/
  - name: twitter:card
    content: summary_large_image
---

# MCP Memory Server: Agent Memory Over the Model Context Protocol

Membrick exposes workspace search, memory management, session recording, and
review tools through the Model Context Protocol (MCP). Its retrieval core
tracks current state, evidence, and supersession across supported clients.

## Tools

Provider adapters expose these workspace search, memory, review, and session
operations. Tool availability can vary slightly by provider; Codex also has the
`membrick_session` tool. See the [MCP tools section of the agent connection guide](connect-agent.md#mcp-tools) for tool availability.

| Tool | What it does |
| --- | --- |
| `search` | Search indexed workspace content and return ranked results with source evidence. |
| `info` | Report workspace and connection information. |
| `list_memories` | List bounded previews of indexed agent memories. |
| `add_memory` | Add a memory you choose to save. |
| `get_memory` | Read one memory by ID. |
| `delete_memory` | Permanently delete a memory owned by the MCP integration. |
| `review_search` | Retrieve a recorded search or page through unreviewed searches. |
| `search_feedback` | Record usefulness and relevance feedback for a search. |
| `enrich_doc` | Add evidence-backed metadata to an indexed source document. |
| `record_session` | Start, stop, or inspect capture-only session recording. |
| `enable_membrick` / `disable_membrick` | Enable or disable retrieval and automatic capture. |
| `membrick_status` | Report independent memory and recording state. |
| `update_membrick` | Install an available Membrick update after host approval. |
| `membrick_session` | Return session information; Codex only. |

MCP calls are explicit agent actions — the model calls `search` when it needs workspace evidence. (For automatic injection without a tool call, see the [lifecycle hooks](claude-code.md).)

The memory behind these tools is the real thing, not a demo: the project's replay harnesses measure it end to end — [Claude Code](claude-code.md) sessions continued in 7.05 s vs 22.47 s on native memory, [Codex](codex.md) in 18.53 s vs 38.13 s, both with a fraction of the input tokens. Same index, same ranking, now callable over MCP.

## How it connects

Each agent adapter exposes a provider-local MCP server over the host client's configured MCP transport. The installers wire it up automatically:

```bash
# Claude Code: adds a local-scope entry under this project's path in ~/.claude.json
./membrick install claude /path/to/project

# Codex: adds a project MCP entry to /path/to/project/.codex/config.toml
./membrick install codex /path/to/project
```

Both provider entries launch Membrick against the selected project. Membrick
loads project behavior from `.membrick/server.toml`; provider configuration
only connects the host client to the local MCP process.

Or run a server directly — for example, to point any MCP-capable client at it:

```bash
./membrick serve claude /path/to/repo
./membrick serve codex /path/to/repo
```

Memory and recording state are project- and provider-scoped. Recording is capture-only: it writes session events but never injects them as memory. Tool and hook failures are fail-open, so the host agent keeps working if the memory layer has a bad day.

## Try it — no building required

Download the release binary for your platform (macOS, Linux, or Windows) from the [releases page](https://github.com/RooAGI/Membrick/releases), then start a server against any project:

```bash
./membrick serve claude /path/to/project
```

We did exactly that against the real Membrick repo and sent a standard MCP `tools/call`. Verbatim exchange:

```json
// client sends
{"jsonrpc": "2.0", "id": 2, "method": "tools/call",
 "params": {"name": "search",
            "arguments": {"query": "why did we drop embeddings", "top_k": 3}}}
```

```json
// server responds — content[0].text parsed for readability,
// excerpts trimmed; scores and statuses verbatim
{"results": [
  {"source": "docs/benchmark.md",  "score": 14.17,
   "semantic_status": "current", "superseded_by": null, ...},
  {"source": "docs/comparison.md", "score": 12.54,
   "semantic_status": "current", "superseded_by": null, ...},
  {"source": "docs/agent-memory.md", "score": 10.88,
   "semantic_status": "current", "superseded_by": null, ...}
]}
```

Ranked results with scores, sources, and a `semantic_status` verdict on every hit — over plain JSON-RPC, callable from any MCP client. To see what's in the index underneath, inspect it directly:

```bash
membrick inspect-index .membrick/claude-memory
```

For the full agent experience (automatic retrieval at session start, capture at session end), use the installer for your agent instead of running the server by hand.

## Frequently asked questions

**Which clients can use the MCP server?**
Any MCP-capable client. Provider MCP adapters are available for Claude Code,
Codex, Gemini CLI, Antigravity CLI, Muse Code, OpenClaw, Hermes, and Roo
Runtime. They speak standard MCP JSON-RPC over the client's configured
transport; Codex also exposes a session information tool.

**Do I need the agent CLIs installed to use it?**
No. MCP and server mode are general Membrick interfaces — an application doesn't need Claude Code, Codex, Gemini CLI, or Antigravity to use the core index, the HTTP API, or the MCP tools. Only the lifecycle hooks are agent-specific.

**How is this different from other memory MCP servers?**
Current-state retrieval. Most memory servers rank by topical or vector similarity; Membrick ranks by what is still true, tracks supersession (new decisions replace old ones without deleting history), and returns evidence with every result. See [stale agent memory](stale-memory.md) for why that matters.

**Is the memory shared between agents?**
Memory state is project- and provider-scoped: the same project indexed once serves every connected client through the same tools.

## Go deeper

- [Connect agent](connect-agent.md) — provider setup and available tools
- [Stale agent memory](stale-memory.md) — why current-state ranking beats similarity ranking
- [Claude Code memory](claude-code.md) / [Codex memory](codex.md) — the full agent integrations
- [Agent memory guide](agent-memory.md) — the bigger picture on persistent agent memory
- [Quickstart](quickstart.md) — the five-minute path from zero to indexed
