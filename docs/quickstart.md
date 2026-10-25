# Get started

Download Membrick, save a few things you want your assistant to remember, and
ask questions in plain language. You do not need to build the project or learn
special memory syntax.

## 1. Download or Update Membrick

### Install Membrick

#### macOS or Linux: official installer

The installer downloads the release for your computer and verifies its
SHA-256 checksum:

```bash
curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh
```

#### Windows: standalone executable

Download `membrick-windows-x86_64.exe` from the [Membrick releases
page](https://github.com/RooAGI/Membrick/releases/latest). Run it from the
download folder in PowerShell. To check the version, run:

```powershell
.\membrick-windows-x86_64.exe --version
```

#### npm

```bash
npm install --global membrick
```

For a Node.js project, install it locally with `npm install membrick`.

#### Python

For an isolated CLI installation, use `pipx`:

```bash
pipx install membrick
```

To add the package to an active Python environment, use
`python -m pip install membrick` instead. The package also provides the Python
memory API.

#### Rust and Cargo

```bash
cargo install membrick --locked
```

The npm package also provides the Node.js memory API.

### Update Membrick

Use the update command that matches how you installed Membrick. You can also
approve the `update_membrick` action in your connected agent, if it is
available.

| Installed with | Update command |
|---|---|
| Official installer (macOS or Linux) | Rerun the official installer command above. |
| Standalone Windows executable | Download the latest `.exe` from the [releases page](https://github.com/RooAGI/Membrick/releases/latest) and replace the old executable. |
| npm global | `npm update -g membrick` |
| npm in a project | `npm update membrick` in that project |
| pipx | `pipx upgrade membrick` |
| pip | `python -m pip install --upgrade membrick` |
| Cargo | `cargo install membrick --locked --force` |

Installations made with the official installer check for updates once per day
when you run Membrick interactively and ask before downloading and replacing
the executable. Add `--no-update-check` to skip the check for one invocation.
MCP and hook processes do not check for updates automatically.

After installation or update, open a new terminal and check the installed
version. On Windows, use the standalone executable command shown above.

```bash
membrick --version
```

After updating, fully quit and reopen Codex, Claude Code, or the agent client
you use. This reconnects its MCP server to the updated Membrick. For details
about update notices and approval, see
[Update notifications](update-notifications.md).

## 2. Migrate from Lint-AI

If you already use Lint-AI in a project, this command installs Membrick and
switches the selected agent's connection while preserving unrelated settings.
Run it from the project directory. For example, to migrate Codex:

```bash
curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/migrate-lint-ai.sh | sh -s -- --agent codex --project .
```

Replace `codex` with `claude`, `gemini`, or `agy` to migrate another supported
agent. The migration preserves existing project data under `.membrick`. When
both `.lint-ai` and `.membrick` exist, non-conflicting files are merged and
conflicting legacy files are kept in `.membrick/migration-archive`. The
project-root `.lint-ai` directory is removed after migration. The old
`lint-ai` executable remains installed; remove it with its package manager
after confirming Membrick works.

Fully quit and reopen the agent. Check its MCP list for `membrick`, then call
a Membrick tool such as `membrick_status` to confirm the connection works.

## 3. Add something to remember

Create a folder for your memories. Save each thing you want to remember in a
plain text or Markdown file. Write naturally; Membrick searches the words and
ideas in the file.

```bash
mkdir -p ~/my-assistant-memory
cat > ~/my-assistant-memory/preferences.md <<'EOF'
I prefer concise answers with a short summary first.
When explaining code, include a small example.
EOF
```

You can add another memory by creating another file or editing an existing one.
These files stay on your computer unless you choose to connect them to another
service.

## 4. Ask a question

Ask Membrick a question about the memories in that folder:

```bash
membrick query "How do I prefer answers to be written?" ~/my-assistant-memory
```

To print a short answer context that you can paste into an AI assistant, use
the `context` command:

```bash
membrick context "How do I prefer answers to be written?" ~/my-assistant-memory
```

Ask a new question whenever you want to look something up. To show more
matches, add `--count 10`.

To see which indexes already exist in the current project, run
`membrick status .`. This only inspects existing index stores; it does not scan
project files.

## 5. Change or forget a memory

Edit a file to change what it says. Run your question again to search the
updated memory. To forget a memory, remove its file:

```bash
rm ~/my-assistant-memory/preferences.md
```

You can also keep a memory by moving its file outside the memory folder. Each
file is one piece of information, so it is easy to update or remove just that
memory.

## Use Membrick inside an AI tool

Membrick connects to Codex, Claude Code, Gemini CLI, Antigravity CLI, OpenClaw,
Hermes, Muse Code, and RooAGI AgentFlow. Choose your agent in the
[agent setup guide](connect-agent.md) for the right installation steps.
Already using Lint-AI? See [Migrate from Lint-AI](#2-migrate-from-lint-ai)
above to switch your agent configuration and project data to Membrick.

## More ways to use Membrick

The command line is the simplest way to try personal memory. Developers can
connect applications through the [HTTP API](server.md), [MCP](mcp.md),
[Python](python-migration-0.3.0.md), [Node.js](node.md), or
[Rust](memory-service-api.md) interfaces.
