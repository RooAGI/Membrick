# Use Membrick with Antigravity CLI

Connect Membrick to a project so Antigravity CLI (AGY) can find earlier
decisions and save useful outcomes as work continues. Project memory is stored
in `.membrick/memory/` and can be shared with other connected agents.

## 1. Install Membrick

On macOS or Linux, download and verify the official release with:

```bash
curl -fsSL https://raw.githubusercontent.com/RooAGI/Membrick/main/scripts/install.sh | sh
```

On Windows, download `membrick-windows-x86_64.exe` from the
[release page](https://github.com/RooAGI/Membrick/releases/latest).

## 2. Enable Membrick for this project

Open a terminal in the project folder and run:

```bash
membrick install agy .
```

This enables AGY to use Membrick in the project. It configures the MCP connection and hooks, installs the project memory skill, and preserves your other AGY settings.
The project is recorded in the Membrick project registry, so the project
manager's `membrick_list_projects` and `membrick_project_status` show it with
the `agy` provider.

AGY reads MCP servers only from the global `~/.gemini/config/mcp_config.json`
and from plugins, and it renames plugin servers instead of letting them replace
a global one. So there is no project-level `membrick` entry, as there is for
Claude Code and Codex. AGY keeps one global `membrick` entry:

- After `membrick` setup it runs the project manager.
- `membrick install agy PATH` points it at that project's memory server. The
  memory tools then appear in every AGY folder, and the manager tools do not.
- Installing a second AGY project moves the entry to that project. The first
  project loses its AGY memory tools and its AGY registration. Its `.membrick`
  data is kept, and the global hooks still run in its folder while that data
  is there.
- Running `membrick` setup again leaves an installed project's entry alone.

The hooks in `~/.gemini/config/hooks.json` find the project from the workspace
AGY reports for each call, so they do nothing in folders that are not Membrick
projects.

On Windows, run this in PowerShell from the project folder (adjust the path if
you saved the executable elsewhere):

```powershell
.\membrick-windows-x86_64.exe install agy .
```

Restart AGY and open the project so it loads the new settings.

At `Stop`, Membrick stores every non-empty transcript record in source order
as an individual `SourceDocument`, including records that are not plain text.
A linked session context is reconciled through `MemoryService`. AGY supplies
the transcript path in its hook payload. Project-local transcripts need no
extra setting; for AGY's centralized transcript directory, configure it in the
project's `.membrick/settings.toml`:

```toml
[hooks]
agy_transcript_dir = "/path/to/.gemini/antigravity-cli/brain"
```

This replaces environment-based transcript path discovery.

## 3. Try it

Ask AGY to look up a decision from earlier work, or continue work in a new
session. Membrick can bring relevant project memory into the conversation and
save useful outcomes as work finishes.

## 4. If memory is not available

Restart AGY and make sure you opened the project where you installed Membrick.
You can check the MCP connection with:

```bash
membrick verify agy .
```

## 5. Remove Membrick from a project

```bash
membrick uninstall agy .
```

This removes the project's AGY hooks and the generated memory skill (only if
you have not edited it), and removes the project's `agy` registration.
Uninstall puts the project manager back in the global `membrick` entry when
that entry serves this project, and leaves it alone when it serves another one.
Project memory in `.membrick/` and the Membrick executable are kept. Restart
AGY afterwards.

For details, see [all agent integrations](agents.md) and
[session recording](session-recording-design.md).
