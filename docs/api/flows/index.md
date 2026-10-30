# Flows

These read-only routes are used by the dashboard. Agents create and advance
runs through the `flow_*` MCP tools. Requests need the shared server token.

| Route | Page |
| --- | --- |
| `GET /api/flows` | [List flows](list.md) |
| `GET /api/flows/{flow_id}` | [Get a flow](get.md) |
