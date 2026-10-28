# Command line

Running `membrick` with no arguments installs the global project manager MCP
for the compiled Codex, Claude Code, and AGY integrations, then displays the
CLI help. Restart those agents after setup. A folder path by itself does not
start a scan.

Each agent gets one MCP server named `membrick`. Outside an installed project
it runs the project manager. Inside an installed project it runs that
project's memory tools.

- Codex: the global `membrick` entry in `~/.codex/config.toml` runs the
  manager. An installed project's `.codex/config.toml` overrides it with the
  memory server. Installing a project keeps the global manager entry.
- Claude Code: the user-scope `membrick` entry in `~/.claude.json` runs the
  manager. An installed project gets a local-scope `membrick` entry, which
  Claude prefers over the user-scope one. If the user-scope `membrick` entry
  is an older memory server with no project root, or a relative root such as
  `.`, setup and project install replace it with the manager. Run
  `membrick install claude PATH` for each project that should keep memory
  tools. If the entry is an older install pinned to one project, setup leaves
  it alone and keeps the old `membrick_manager` entry. Reinstall that project
  to move its entry to local scope, then run `membrick` again.
- AGY: AGY reads MCP servers only from the global
  `~/.gemini/config/mcp_config.json` and from plugins, so there is no
  project-level entry that can override the global one. The global `membrick`
  entry runs the manager until an AGY project is installed, and that project's
  memory server after that, in every folder. While a project is installed, AGY
  has no manager tools, and running `membrick` setup again leaves the project
  entry alone. Installing another AGY project moves the entry to it and
  removes the first project's AGY registration. AGY hooks are global and find
  the project from the AGY workspace, so they still run in the first project's
  folder while it has `.membrick` data.
  `membrick uninstall agy PATH` puts the manager entry back when the entry
  serves `PATH`.

Setup removes the old separate `membrick_manager` entry when it can. The
manager can list installed projects, check project status, and install or
remove Membrick for an explicit Codex, Claude Code or AGY project. It does not
expose project memory tools. Restart the agent after installing a project so
it loads the memory tools. To enable project memory, run
`membrick install codex PATH`, `membrick install claude PATH` or
`membrick install agy PATH` from a terminal or ask the global manager to
install it for that project. Installations for all three providers are
recorded in the user's Membrick config directory at
`membrick/installed-projects.json`; uninstall removes only that provider's
registration and leaves project memory data in place.

## Project commands

| Command | Purpose |
|---|---|
| `membrick status [PATH]` | Show persisted index stores under `PATH/.membrick`; does not scan project files |
| `membrick review [PATH]` | Run project lint checks with the search index prepared |
| `membrick analyze [PATH]` | Analyze project structure |
| `membrick index [PATH] [--redacted]` | Build or refresh an index |
| `membrick query <TEXT> [PATH] [--count N]` | Search project documents |
| `membrick context <TEXT> [PATH]` | Prepare LLM context from project documents |
| `membrick recall <TEXT> [PATH] [--count N]` | Search indexed documents and recorded memories |
| `membrick recall serve [PATH]` | Serve recall requests over newline-delimited JSON |
| `membrick inspect-index <PATH> [--view VIEW]` | Inspect one existing index |
| `membrick session promote <ID> [--provider PROVIDER] [--root PATH]` | Promote a recorded session into project memory |
| `membrick session replay <ID> [--provider PROVIDER] [--root PATH]` | Replay a recorded session, optionally toggling Membrick |
| `membrick export graph <FORMAT> [PATH]` | Export a document, chunk, or entity graph |
| `membrick export ontology [PATH]` | Export ontology data |

`PATH` defaults to the current folder only after a command is supplied. The
status command lists persisted stores and their document, record, publication,
revision, and segment counts. It does not scan project files.

### Keep personal notes as files

For a simple command-line workflow, save each note as a Markdown file and
search the folder with `query`:

```bash
mkdir -p ~/my-assistant-memory
cat > ~/my-assistant-memory/preferences.md <<'EOF'
I prefer concise answers with a short summary first.
When explaining code, include a small example.
EOF

membrick query "How do I prefer answers to be written?" ~/my-assistant-memory
membrick context "How do I prefer answers to be written?" ~/my-assistant-memory
```

`query` reads the current files, prints JSON matches, and reuses a cached index
when the folder has not changed. To include more matches, add `--count 10`.
Edit the Markdown file to change a note, or remove it to forget that note:

```bash
$EDITOR ~/my-assistant-memory/preferences.md
rm ~/my-assistant-memory/preferences.md
```

This workflow treats files as notes. It does not create or edit captured memory
records. Use the [HTTP API](api/index.md) or a language SDK to manage those
records by ID.

Graph formats are `dot`, `json`, and `cytoscape-html`. Graph levels are `doc`,
`chunk`, and `entity`. Index inspection views are `summary`,
`source-documents`, `records`, and `segments`.

## Provider commands

```sh
membrick install <provider> [PATH]
membrick serve <provider> [PATH]
membrick verify <provider> [PATH]
membrick uninstall <provider> [PATH]
membrick hook <provider> <event> [PATH]
membrick statusline <claude|codex> [PATH]
```

Uninstall currently supports `claude`, `codex` and `agy`. It removes only the
configuration Membrick wrote for that project and keeps project memory data
and the executable. A generated memory skill is removed only if it is
unchanged. Running it again is safe. For AGY, if the global `membrick` entry
serves that project, uninstall replaces it with the project manager entry; an
entry for another project is left alone. Provider-specific paths can
be overridden with `--provider-config`, `--provider-settings`,
`--provider-skills-dir`, `--provider-hooks-dir`, and `--provider-plugin-dir`.
Use `--force-skill` with install to replace a user-modified provider skill.
`statusline` currently supports `claude` and `codex`. The MCP verification
timeout is configurable with `--timeout`.

## Advanced configuration

Index extraction and chunking defaults are configured in the project config
file. Older single-dash command modes and low-level diagnostic switches remain
accepted for compatibility, but are omitted from normal help. Use the commands
above for supported CLI workflows. `--config PATH` selects a config file and
`--no-update-check` skips the interactive update check for one invocation.
