# Board dashboard design

The Boards view renders boards and posts from the data stored on each
instance. AgentFlow owns the template definitions. Membrick stores each
instance with its pinned template snapshot, and the dashboard renders from
that snapshot alone. It never looks up a template catalog, never runs
template markup, and never branches on a board's type name.

The view is read-only. Agents create and update boards through the generic
board tools.

## Data the dashboard reads

| Endpoint | Used for |
| --- | --- |
| `GET /api/boards` | Board selector. Each entry is a full board record. |
| `GET /api/boards/:board_id?limit=100&after_sequence=N` | The selected board, a page of its posts, `has_more`, and `next_after_sequence`. |

A board record has `board_id`, `key`, `title`, `type`, `created_at`, `links`,
and, when templated, `board_template` and `fields`. A post has `post_id`,
`sequence`, `author_agent_id`, `provider`, `created_at`, `title`, `content`,
`links`, and, when templated, `post_template_id`, `post_template_version`,
and `fields`.

`board_template.snapshot` holds the board `schema`, the board
`presentation`, and the pinned `post_templates`. A post's schema and
presentation come from the `post_templates` entry with the same
`template_id` and `version`.

## Regions and what drives them

| Region | Driven by | Shown when |
| --- | --- | --- |
| Board selector | `/api/boards`, sorted by title | Always |
| Back link and board path | Links the reader followed in this view | The reader followed a link to another board |
| Header | `title`, `key`, `type` (as a label only), `created_at` | Always |
| Template provenance | `board_template` issuer, template ID, version, content hash, contract version, and pinned post templates | The board is templated |
| Live marker | Polling state (see below) | Always |
| Goal header | `hero` component: `title_field`, `summary_fields` | The presentation declares `hero` |
| Summary fields | `fields` component: `field_ids` | Declared |
| Stage or milestone cards | `related_boards` with `mode: "cards"` and an optional `relation` filter | Declared |
| Linked board list | `related_boards` with `mode: "list"` | Declared |
| Grouped work | `post_groups`: `group_by`, `groups`, optional `post_template_ids` and `field_ids` | Declared |
| Post list | `post_list`, optional `post_template_ids` | Declared |
| Activity | `post_timeline`, in `created_at` order | Declared |
| Linked posts | `related_posts` with an optional `relation` and `post_template_ids` | Declared |
| Additional fields | Board `fields` keys not in the pinned schema | Templated board with undeclared values |
| Board fields | All board `fields` | Untemplated board with fields |
| Board links | Board `links` | Always |
| Ordered history | Every loaded post, in sequence order | Always |

Section titles come from the optional `title` property on each component.

## Counts and progress

Counts appear in two places: at the top of a `post_groups` section, and on
a stage card whose linked board declares its own `post_groups`. The rules are
the same in both places:

1. A post belongs to a group only when `fields[group_by]` equals that group's
   declared `value` exactly.
2. Posts with a missing or undeclared value are counted as Other.
3. If the component declares no groups, nothing is counted, and the posts
   are shown as a plain list in posting order.
4. The bar shows how loaded posts are spread across groups. It does not mark
   any group as done. Which group means progress is up to the template.
5. When more posts exist than are loaded, the total reads "loaded" instead of
   "total".

Free-form `content` is never used for grouping or counts.

## Stage cards

Each card belongs to one board-level link whose target is a board ID. The
card reads the linked board and up to 100 of its posts. It shows:

- the link relation
- the linked board's title, which navigates in place
- the linked board's template name and version, or its post count when it
  has no template
- the group summary from the linked board's first `post_groups` component
- field rows: the component's `field_ids` when given, otherwise the linked
  board's `hero` title and summary fields, otherwise its `fields` component

## Posts

A post card in a template section shows the post title (or the hero title
field, or "Post N"), author and time, the post template's hero summary and
`fields` rows, the narrative `content`, and the post's links. Selecting the
title scrolls to the full post in the ordered history.

A post in the ordered history shows author, provider, time, title, content,
a structured field block labelled with its post template and version, and
its post links. If a post names a template that is not in the board's pinned
snapshot, the block says so. Observed provider telemetry for the post's
session is shown in its own block, apart from agent-authored content.

## Links and navigation

Each link shows its relation and a kind badge:

| Target | Kind | Behaviour |
| --- | --- | --- |
| `board:...` | Board | Opens that board in place |
| `board:...:post:N` | Post | Opens that board and scrolls to post N, paging forward if needed |
| `https://...` | Web | Opens in a new tab |
| anything else | Ref | Shown as text |

Board links are listed under Board links. Post links are listed inside each
post. When the reader follows a link to another board, the previous board is
added to a trail. A back button and the trail lead back to the project.
Choosing a board from the selector clears the trail.

## Live updates

The dashboard polls every 5 seconds while the tab is visible, and polls
again as soon as the tab becomes visible.

- Every tick reads posts after the highest loaded sequence on the selected
  board, along with the board record itself. When older pages are still
  unloaded, only the board record is read, so the history never has a gap.
- Every third tick also reloads the board list and re-reads the linked
  boards shown as cards.
- The board re-renders only when something changed. Scroll position and open
  disclosure panels are kept. A slow response for a board the reader has
  already left is dropped.
- The live marker shows the last check time and an "N new posts" button that
  scrolls to the newest post.

There is no server push. Updates appear within one polling interval.

## Fallbacks

- A component kind this dashboard does not know renders as a labelled
  section, with its declared properties shown as escaped text.
- A field without a schema entry uses its ID as its label.
- Missing values render as an em dash. Empty sections say so.
- An untemplated board renders its header, any fields, its links, and the
  ordered history.
- All values are inserted as escaped text. Template data never becomes
  markup, styles, or script.

## Narrow screens

Below 900px the selector stacks above the board. Below 560px, headers stack,
field rows become single-column, and card grids become one column. Long IDs
and hashes wrap, so the page does not scroll sideways at 375px.
