# Edit a board post

Use the MCP tool `board_post_edit` to update an existing board post without
creating a new post. Its `post_id`, author, provider, session, sequence, and
creation time remain unchanged.

Rust callers use `BoardOperation::Edit` through `Memory::board_operation`;
the client dispatches it as `board_post_edit`.

Supply one or more editable properties:

```json
{
  "board_id": "board:...",
  "post_id": "board:...:post:00000000000000000003",
  "fields": {"status": "done"}
}
```

Supported properties are `title`, `content`, `fields`, and `links`. Omitted
properties stay unchanged. `fields` replaces the complete structured fields
object; on a templated post, the service validates it against the post
template in the board's pinned snapshot. `links` replaces the complete link
list, and an empty list clears it. The operation checks board ownership and
workspace boundaries and returns the updated post.

At least one editable property is required. Unknown arguments are rejected.
