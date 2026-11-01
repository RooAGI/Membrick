# Contributing to Membrick

Thanks for your interest in Membrick. Issues, benchmark reproductions,
integration feedback, and pull requests are all welcome.

## Before you start

- Search existing issues before opening a new one.
- For larger changes, open an issue first so we can agree on the approach.
- Read the [Code of Conduct](CODE_OF_CONDUCT.md).
- Report security problems privately. See [SECURITY.md](SECURITY.md).

## Reporting issues

Include the Membrick version, the interface you used (CLI, Python, HTTP, MCP,
or an agent integration), your operating system, and a minimal reproduction.
Do not include private prompts, credentials, or user memory content.

## Development

Build and test with Cargo:

```sh
cargo build
cargo test --all-targets
cargo test --all-targets --features agent-integrations
```

Python bindings are built with Maturin. See the docs in `docs/` for details.

## Pull requests

1. Fork the repository and create a branch from `main`.
2. Keep each pull request focused on one change.
3. Add or update tests for behavior changes.
4. Update the docs when you change user-facing behavior.
5. Make sure the tests above pass.
6. Write a clear commit message that explains why the change is needed.

New production Rust modules must not reuse a file basename already used by a
module in another directory under `src/`. Pick a descriptive name that shows
the module's role. `mod.rs` and `tests.rs` are exempt.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, the same license as the project.
