# Boards

These read-only routes are used by the dashboard. Agents write through the
`board_*` MCP tools. Requests need the shared server token.

To edit a post in place, use the MCP tool [`board_post_edit`](post-edit.md).

| Route | Page |
| --- | --- |
| `GET /api/boards` | [List boards](list.md) |
| `GET /api/boards/{board_id}` | [Get a board](get.md) |

Boards are scoped to the server's project root. Board IDs look like
`board:{owner}:{workspace}:{key}` with `%` and `:` escaped inside each part.
Treat them as opaque and take them from the list route.
