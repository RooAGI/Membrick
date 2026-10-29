# Questions & Answers

## About Membrick

??? question "What is Membrick?"
    Membrick gives AI agents a persistent memory layer for useful preferences,
    decisions, and project history. It retrieves relevant context and helps
    distinguish current information from older history. See
    [How Membrick remembers](agent-memory.md).

??? question "How is Membrick different from chat history?"
    Chat history keeps a record of conversations. Membrick makes useful
    context searchable across sessions and helps the agent recognize when a
    newer decision replaces an older one, while preserving the history. See
    [How Membrick remembers](agent-memory.md).

## Data and privacy

??? question "Where does Membrick keep my data?"
    In the default local setup, project memory lives in `.membrick/memory` in
    your project. You can inspect and edit those files directly. Membrick also
    supports a separately configured HTTP server for applications that need a
    remote workspace. See [storage and server options](server.md).

??? question "Does Membrick need an LLM API key or a cloud account?"
    Local memory search does not need a separate model API key or Membrick
    account. When used inside an AI agent, Membrick connects to the agent you
    already use. Remote server deployments can have their own authentication
    settings.

??? question "What conversation data does Membrick capture?"
    Capture depends on the agent. Claude Code and Codex preserve parsed
    transcript messages as searchable turn records; other integrations capture
    the context available in their provider events. Optional session recording
    archives hook events separately. See [agent integrations](agents.md).

??? question "Can I see or edit what Membrick remembers?"
    Yes. Project memories are files under `.membrick/memory`. You can inspect,
    edit, add, or remove them with your usual editor. See
    [How Membrick remembers](agent-memory.md).

## Installation and use

??? question "Which AI agents can use Membrick?"
    Membrick supports Codex, Claude Code, Gemini CLI, Antigravity CLI, Muse
    Code, OpenClaw, Hermes, and RooAGI AgentFlow. Follow the
    [agent setup guide](connect-agent.md) for your client.

??? question "Can I use Membrick with more than one AI agent?"
    Yes. Supported agents in a project share its `.membrick/memory` store, so
    information saved through one integration can be available to another.
    See the [supported agent integrations](connect-agent.md).

??? question "How do I install or update Membrick?"
    Follow the [Quickstart](quickstart.md#1-download-or-update-membrick) for
    standalone, npm, Python, and Rust installation and update commands. Restart
    your agent after updating so it reconnects to the new executable.

??? question "How do I migrate from Lint-AI?"
    Follow [Migrate from Lint-AI](connect-agent.md#migrate-from-lint-ai) to
    install Membrick, update your agent connection, and migrate project data.

??? question "What happens when a decision changes?"
    Membrick can keep the earlier information as history while preferring the
    newer supported decision for a current-state question. See
    [semantic supersession](semantic-supersession.md).
