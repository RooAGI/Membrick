# Server configuration

The HTTP server and project resident read settings from a TOML file. The
default path is `.membrick/server.toml` under the selected project root, or
under the current directory when no project root is selected. Pass
`--settings PATH` to use another file. Explicit command-line options take
precedence over values in the file. The server does not read its settings from
environment variables. If the selected file is missing, the server uses
defaults and does not create it. Project installation and the SDK's explicit
`Memory::init` operation create `.membrick/server.toml` for a project.

`--settings PATH` is for standalone server configuration. A project resident
must use its project's `.membrick/server.toml`, because SDK clients discover
the optional HTTP listener from that project file.

Project installation creates `.membrick/server.toml` with local defaults and
preserves an existing file. A managed resident reads this same file. The
standalone HTTP server uses `bind`; a managed resident uses
`resident_http_bind` for its optional HTTP listener while provider tools and
hooks normally connect through local IPC.

The standalone server loads the selected file once and uses it for server
options, logging, and the primary project's scan exclusions. When serving
additional allowed workspaces, each workspace uses its own
`.membrick/server.toml` for `ignore_paths`.

## Dashboard URL and route

There is no separate dashboard listener or enable/disable setting. The
dashboard is served by the HTTP server at the fixed path `/dashboard`, on the
same listener as the API. For a standalone server, `bind` determines the
address and port; for a resident's optional HTTP listener,
`resident_http_bind` determines them.

The generated `dashboard_url` is a convenience URL for opening the local
dashboard. For example:

```toml
resident_http_bind = "127.0.0.1:8465"
dashboard_url = "http://127.0.0.1:8465/dashboard"
```

`dashboard_url` does not configure the listener or change the route. Editing
only that value will not expose the dashboard at a different address. For a
standalone listener at `127.0.0.1:8080`, open
`http://127.0.0.1:8080/dashboard`. For remote access, configure DNS, HTTPS,
and the public URL at the reverse proxy; for example, proxy
`https://memories.example.com/dashboard` to the server's private listener.
Keep the server-side listener and API authentication configured as described
in [Run remotely](server.md#run-remotely).

## Example

This example is for a local standalone server. Keep secrets in a protected file
or secret manager instead of committing them to a project repository.

```toml
bind = "127.0.0.1:8080"
project_root = "/path/to/project"
index = "/var/lib/membrick/memory-index"
server_token_file = "/path/to/server-token"
ignore_paths = ["node_modules", "target", "dist", "build", "vendor", "coverage", ".git", "venv", ".tox", "site-packages"]

# Optional: route beyond the default five candidate segments.
adaptive_segment_max_n = 32

# Optional query-time recency ranking policy.
[freshness]
mode = "relative"
half_life_days = 30.0
weight = 0.25
```

Start it with:

```bash
cargo run --release --bin server -- --settings .membrick/server.toml
```

For a remote server behind HTTPS and an OAuth/OIDC proxy, see
[Run remotely](server.md#run-remotely). Membrick does not implement the OAuth
login flow; the proxy must also provide a bearer token accepted by the server.

## Settings

### Paths and listeners

| Setting | Meaning | Default or behavior |
|---|---|---|
| `bind` | Address for the standalone server's HTTP listener. | `127.0.0.1:8080`. Non-loopback addresses require `allow_non_loopback = true` and configured authentication. |
| `project_root` | Project whose files and `.membrick` data the server uses. | The current directory, unless selected by `--project-root`. |
| `index` | Explicit backing index directory. | If omitted, Membrick discovers the project indexes under `.membrick`; use this when the server needs a specific store path. |
| `resident_http_bind` | Optional HTTP listener for the managed project resident. | If omitted, the resident exposes no HTTP listener. Project installation selects a loopback address. It is separate from the standalone server's `bind`. |
| `resident_http_auto_port` | If the resident's configured port is occupied, allow it to select and save another port. | Project installation sets this to `true`; otherwise it defaults to `false`. Automatic selection tries the resident port range `8465`–`9999` and updates `resident_http_bind` and `dashboard_url`. |

The project installer writes `dashboard_url` so users can find the local
dashboard. If automatic port selection chooses another port, the resident
updates this URL along with `resident_http_bind`.

### Authentication and tenant scope

| Setting | Meaning | Default or behavior |
|---|---|---|
| `server_token` | Shared API bearer token. | Unset. Prefer `server_token_file` for deployed servers. |
| `server_token_file` | File containing the shared API token. | Unset. If both token settings are present, the file value is used. An explicit `--server-token` overrides settings-file values. |
| `jwt_secret` | Secret used to validate per-user HS256 JWTs. | Unset. JWTs require non-empty `sub` and valid `exp` claims. When configured, JWT authentication takes precedence over shared-token authentication. |
| `tenant_id` | Restricts requests to one configured tenant/user scope. | Unset. Requests with a different `user_id` are rejected. |
| `allow_non_loopback` | Allows `bind` or `resident_http_bind` to listen beyond loopback. | `false`. Requires `server_token`, `server_token_file`, or `jwt_secret`; cannot be combined with `--allow-unauthenticated`. |

For remote access, keep the server on a private interface or network and
terminate TLS at a trusted proxy. See [Run remotely](server.md#run-remotely)
for the proxy and OAuth/OIDC requirements.

### Project scanning and retrieval

| Setting | Meaning | Default or behavior |
|---|---|---|
| `ignore_paths` | Path fragments, relative to the project root, excluded from project scanning. | Defaults to `node_modules`, `target`, `dist`, `build`, `vendor`, `coverage`, `.git`, `venv`, `.tox`, and `site-packages`. Setting the list replaces the defaults; `[]` skips no paths. |
| `adaptive_segment_max_n` | Maximum candidate segments for adaptive routing. | Fixed top-five routing is the default. Set a value greater than `5` to let routing expand when the first five candidates lack enough query evidence. |
| `[freshness].mode` | Recency policy: `relative`, `legacy`, or `disabled`. | `relative`; the default affects `CurrentState` queries only. |
| `[freshness].half_life_days` | Relative-freshness half-life, in days. | `30.0`. Must be finite and greater than zero. |
| `[freshness].weight` | Maximum relative-freshness boost. | `0.25`. Must be between `0.0` and `1.0`. |

Freshness is applied at query time and does not rebuild the index. The same
policy applies to ordinary and structured retrieval.

## Capture-only hook mode

The setting for keeping provider hook capture active while disabling automatic
memory retrieval and injection is **not** part of `server.toml`. Put it in the
separate `.membrick/settings.toml` file:

```toml
[hooks]
observe_only = true
```

This leaves capture enabled for supported integrations but prevents lifecycle
hooks from automatically adding recalled memories to prompts or tool
decisions. The default is `false`. Explicit MCP searches still work, and the
Membrick integration must still be enabled for its capture hooks to run. Muse
hooks already work in capture-only mode. See [Capture without automatic
retrieval](agents.md#capture-without-automatic-retrieval) for provider details.

This project setting is separate from session recording. Starting
`record_session` records hook events in capture-only mode as a separate,
opt-in session archive; see the [session recording design](session-recording-design.md).

### Logging

| Setting | Meaning | Default or behavior |
|---|---|---|
| `[logging].level` | Minimum Membrick log level: `off`, `error`, `warn`, `info`, `debug`, or `trace`. | Project installation writes `info`. Read from the project's `.membrick/server.toml`; background roles default to `info` and interactive commands to `warn` when no valid setting is present. |

## Generated settings and command-line options

Project installation creates these local entries in `.membrick/server.toml`:

```toml
bind = "127.0.0.1:8080"
project_root = "/path/to/project"
resident_http_bind = "127.0.0.1:8465"
resident_http_auto_port = true
dashboard_url = "http://127.0.0.1:8465/dashboard"
ignore_paths = ["node_modules", "target", "dist", "build", "vendor", "coverage", ".git", "venv", ".tox", "site-packages"]

[logging]
level = "info"
```

The installer chooses and records the actual project root and resident port;
the values above are examples.

The selected resident port can differ. A resident port conflict can also
rewrite `resident_http_bind` and `dashboard_url`. Do not replace the generated
resident listener with a public address unless remote access is intentional;
non-loopback listeners require explicit opt-in and authentication.

The TOML settings schema is smaller than the command-line interface. For
example, `refresh_interval_ms`, `refresh_batch_size`, and
`checkpoint_interval_seconds` are command-line options, not TOML settings.
Consult `membrick-server --help` for the supported flags. The
[HTTP server guide](server.md) explains the API and server modes.
