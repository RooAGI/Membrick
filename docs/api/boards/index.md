# Boards

These read-only routes are contributed by the Flowboard Board/Post extension
and used by the dashboard. Agents write through the
[Board/Post extension tools](../../extensions/board-post.md). Requests need
the shared server token.

To edit a post in place, use the MCP tool [`board_post_edit`](post-edit.md).

| Route | Page |
| --- | --- |
| `GET /api/boards` | [List boards](list.md) |
| `GET /api/boards/{board_id}` | [Get a board](get.md) |

Extension change notices for boards and other capabilities are available on
the [generic extension event stream](../extensions/events.md).

Boards are scoped to the server's project root. Board IDs look like
`board:{owner}:{workspace}:{key}` with `%` and `:` escaped inside each part.
Treat them as opaque and take them from the list route.
