<!-- membrick:memory-policy:start -->
## Membrick memory policy

For requests about project history, prior decisions, architectural choices,
earlier work, or "why" something was implemented, call the `membrick` MCP
server's `search` tool before reading files or searching the repository.

1. Call `search` with a concise query describing the missing context.
2. Read the returned sources before opening files yourself.
3. Say plainly when memory returned nothing, and only then fall back to
   reading the repository.

Recorded sessions are memory of this project, not documentation of it: a
finding can be out of date. Check a conclusion against the code before relying
on it, and prefer the most recent one when two disagree.
<!-- membrick:memory-policy:end -->

## User preferences

Prefer tool-native configuration over environment variables when a clean,
supported alternative exists. For Python extension packaging, use Maturin's
supported workflow and let it manage PyO3 extension linker settings.

In written responses, avoid `---` separators and keep punctuation and phrasing
simple.

## Rust source naming

Do not introduce new production Rust modules with a basename already used by a
module in another directory under `src/`. Use a descriptive filename that
identifies the module's role. Existing duplicate basenames may remain unless
the affected code is being refactored. Conventional `mod.rs` and `tests.rs`
files are exempt.
