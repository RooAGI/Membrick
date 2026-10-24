# ChatGPT and AgentFlow remote MCP architecture

For the user journey, exact ChatGPT tool responsibilities, and write
permissions, see [ChatGPT AgentFlow template contract](chatgpt-agentflow-template-contract.md).

Status: architecture recommendation, 2026-10-20. This note describes the
current Membrick boundary and the work needed to connect ChatGPT to
AgentFlow-owned templates and a user's Membrick workspace.

## Current implementation boundary

Membrick's resident is a local workspace owner. The normal HTTP bind is
`127.0.0.1:8080`; installed residents use a loopback port for dashboard/API
access and prefer a user-private Unix socket or named pipe for provider
adapters. Provider MCP adapters speak MCP to their host over stdio, then call
the resident over local IPC, loopback HTTP, or the local file queue. See
[`local-ipc-architecture.md`](local-ipc-architecture.md).

The `/provider-memory/mcp/tools/call` route is an internal HTTP bridge for
bundled provider adapters. It accepts a Membrick request containing workspace,
provider, tool, and arguments; it is not itself the standard remote MCP
endpoint ChatGPT should connect to. Only `tools/call` and document upsert are
exposed on that route family; it has no remote MCP initialize, tool discovery,
or Streamable HTTP session endpoint.

Authentication is deliberately workspace-scoped. The provider MCP routes
require the configured server token. A user JWT is rejected for workspace
operations, and tenant mode disables these workspace MCP routes. The request
also supplies a workspace path, so a future remote gateway must bind that path
to an authenticated user's authorized workspace rather than trust a
model-supplied value. The server has an explicit non-loopback opt-in, but this
integration must not use it to expose a resident listener.

## Recommendation

For a user connecting ChatGPT to their own development workspace, use
ChatGPT's **Secure MCP Tunnel** with a small local MCP gateway. ChatGPT's
current custom MCP connection flow supports either a public HTTPS MCP endpoint
or Secure MCP Tunnel for a private-network or developer-machine server; see
[Connecting custom MCP servers in ChatGPT](https://help.openai.com/en/articles/12584461-connecting-custom-mcp-servers-in-chatgpt).
The tunnel keeps the resident private and avoids publishing a home or
development machine's HTTP port. Do not tunnel the resident's HTTP listener or
dashboard directly.

The local gateway should expose standard MCP over the transport supported by
the tunnel, authenticate the ChatGPT connection using the supported
authorization flow, and present only the AgentFlow workflow tools. It can call
the AgentFlow template catalog over HTTPS and access Membrick through the
resident's local IPC. The flow and board instance continue to carry the pinned
AgentFlow template ID, version, hash, and snapshot. The gateway binds to a
configured workspace locally; it does not accept arbitrary workspace paths
from tool arguments.

Keep the initial tool surface narrow:

| Tool | Access | Effect |
| --- | --- | --- |
| `template_search`, `template_get` | Read | Browse and inspect immutable AgentFlow templates. |
| `flow_start` | Write, require user approval | Resolve and pin the selected template, then create the FlowRun and board instances. |
| `flow_status`, `board_read` | Read | Inspect the active run and its board. |
| `board_post` | Write, require user approval | Add a coordinator update to the authorized run's board. |

The gateway must verify the user's workspace authorization before starting a
flow or writing a post. It must also apply idempotency keys, record an audit
trail, and ask ChatGPT to obtain approval for state-changing tools. Template
discovery and preview must not create boards or runs.

## Hosted service alternative

A hosted AgentFlow MCP endpoint is the right long-term choice for shared,
managed workspaces and team administration. It can centralize OAuth, policy,
auditing, and template discovery. It cannot safely reach a user's local
resident by itself. A hosted deployment needs either hosted Membrick storage
for that workspace or an authenticated outbound connector running beside the
resident. It also needs explicit tenant/workspace isolation and scoped
authorization; the existing Membrick server token is a workspace-wide secret,
not a per-user OAuth credential.

Use the tunnel-backed local gateway for an individual user's local workspace
experience. Use a hosted gateway when AgentFlow and Membrick own the workspace
runtime. In both cases, keep the resident loopback/IPC-only and retain its
current auth checks. Do not use `--allow-non-loopback`, `--allow-unauthenticated`,
or a public reverse proxy to the resident.

## Work still required

1. Implement the local MCP gateway that composes AgentFlow's remote template
   APIs with Membrick's local resident IPC. The current provider MCP adapter
   is not yet a ChatGPT remote-gateway package.
2. Confirm the supported ChatGPT Secure MCP Tunnel setup and authentication
   flow for distributing the gateway to users. Use an OAuth connection where
   supported; do not distribute a shared server token to ChatGPT clients.
3. Decide whether the first release targets personal local workspaces,
   managed hosted workspaces, or both. A hosted release additionally requires
   a workspace connector or hosted Membrick runtime.
4. Keep `flow_start` and `board_post` approval-gated, and test that template
   browse/preview has no write side effects.
