# Release checklist

Use this checklist for every Membrick release. Complete the preparation and
benchmark work before opening the release PR. Merge the PR before creating its
`vX.Y.Z` tag. Check each item or record why it does not apply.

## Release files and what they control

Update only the files for components included in the release. Keep package
versions aligned with the tag, and regenerate each lockfile with its package
manager.

| Release area | Files to update | Purpose |
|---|---|---|
| Rust CLI and library | `Cargo.toml`, `Cargo.lock` | Crate version and locked dependency/package metadata |
| Python package | `pyproject.toml`, `uv.lock` | Python distribution version and lockfile |
| Node package | `bindings/node/package.json`, `bindings/node/package-lock.json`, `bindings/node/Cargo.toml`, `bindings/node/Cargo.lock` | npm package and native binding versions |
| Node platform packages | `bindings/node/package.json` (`napi.targets` and `optionalDependencies`) and `.github/workflows/node-publish.yml` | The workflow builds and publishes the five platform packages named in the root package manifest |
| OpenClaw plugin | `src/integrations/openclaw/plugin/package.json`, `src/integrations/openclaw/plugin/openclaw.plugin.json` | Plugin package version and discovery metadata |
| Helm chart | `charts/membrick/Chart.yaml` | Chart version and application version |
| Release notes | `docs/releases/X.Y.Z.md` | User-visible changes, upgrade steps, compatibility, and known issues |
| Website content | `docs/index.md` for homepage claims; relevant `docs/*.md` pages for guides and benchmarks; `mkdocs.yml` for navigation | Published documentation site. `README.md` updates the GitHub repository page; it does not update the website homepage |
| Benchmark evidence | `comparison/results/<run>.json` or `.json.gz`, plus `docs/benchmark-results.md` | Reproducible report artifacts and the human-readable summary |
| Publishing workflows, when needed | `.github/workflows/release.yml`, `crate-publish.yml`, `python-publish.yml`, `node-publish.yml`, `docs.yml` | Build and publish behavior. Change these only when the workflow itself needs a fix |

GitHub Actions permissions, environments, npm credentials, and registry trusted
publisher settings are configured in GitHub or the package registries, not in
these repository files. Verify them as separate release prerequisites.

## Publish sequence

1. Update the applicable version files, release notes, benchmark evidence, and
   website pages in one release PR. Update `README.md` separately when its
   repository overview or benchmark summary needs to change.
2. Review the file map above, generated manifests, lockfiles, package contents,
   and version consistency. Run the required checks and benchmarks, then merge
   the PR to `main`.
3. Create and push one annotated `vX.Y.Z` tag on the merged commit. The tag
   starts the GitHub release, crates.io, PyPI, and npm publication workflows.
4. Confirm every workflow validates the same tag version and finishes
   successfully. Check the GitHub release assets and every registry package,
   including platform-specific npm packages.
5. Confirm the documentation workflow deploys the merged `docs/` changes.
   README changes alone do not trigger or update the website homepage.
6. Record the merged commit, tag, workflow run links, published package
   versions, and any exceptions in the release record.

## v0.7.1 release candidate

Use this section for the current 0.7.1 patch candidate. Version metadata and
release notes are in the working tree; publication is a separate step after the
release PR is merged and all required checks pass.

- [x] Choose `0.7.1` as the compatible patch release version.
- [x] Add `docs/releases/0.7.1.md` with user-visible changes, upgrade guidance,
  storage compatibility, and provider setup notes.
- [x] Align active Rust, Python, npm, native Node, platform package, OpenClaw,
  Helm, citation, Docker example, and documentation-navigation versions.
- [ ] Confirm `v0.7.1` and all package versions are unused in GitHub Releases,
  crates.io, PyPI, npm, and the five npm platform packages immediately before
  publishing.
- [ ] Confirm the 0.7.1 release scope, migration behavior, compatibility limits,
  and known issues against the final merged candidate.
- [ ] Verify account-side PyPI and crates.io Trusted Publisher settings,
  repository Actions permissions, and required environments.
- [ ] Keep unrelated workspace changes out of the release PR.
- [ ] Run affected provider lifecycle scenarios and verify that concurrent
  clients share one resident without losing writes.
- [ ] Measure the resident memory and idle CPU changes described in the release
  notes on a documented host and candidate build.
- [ ] Run retrieval and indexing regression checks if the release changes those
  paths; record the exact commit, configuration, dataset, commands, and results.
- [ ] Review all benchmark results against stated regression limits and explain
  accepted trade-offs.
- [ ] Save benchmark commands and artifacts. Redact secrets and obtain consent
  before retaining external users' transcripts.
- [ ] Review the final release diff and confirm every included change belongs
  in 0.7.1.

## In the release PR

- [ ] Full Rust test matrix passes, including supported agent integration
  features. Before the test-only lock-wait adjustment, an all-targets run had
  1,096 passes and one shared board-lock timeout under parallel load. After the
  adjustment, the focused test and full library suite passed (1,097 tests). A
  later all-targets run was interrupted, so this gate remains open.
- [ ] Agent install contract and Unix and Windows installer checks pass.
- [ ] Python binding tests and wheel builds pass on supported platforms.
- [ ] Node binding tests and native builds pass on every supported target.
- [ ] Security audit passes, or each finding has a documented disposition.
- [ ] Strict MkDocs documentation build and link checks pass locally; confirm
  the docs workflow on the updated PR head as well.
- [ ] Version metadata matches across release components and matches the
  proposed `vX.Y.Z` tag.
- [ ] `cargo package --locked --list` and `cargo publish --dry-run --locked`
  succeed for the Rust crate.
- [ ] Package contents include the expected CLI, native libraries, licenses,
  and metadata for each published package.
- [ ] Include benchmark commands, artifacts, and a concise results summary in
  the PR. Explain any waived benchmark and its reason.
- [ ] All required reviews are complete and the release PR is merged to `main`.

## After the release PR is merged

- [ ] Confirm `main` contains the merged release commit and required checks pass
  on that commit.
- [ ] Create an annotated `vX.Y.Z` tag on the merged commit. Do not tag an
  unmerged release branch.
- [ ] Push the tag once. Confirm the release, crate, Python, and Node workflows
  all started from that tag and each workflow validates the same version.
- [ ] Wait for all build jobs to pass before treating the release as available.
- [ ] Confirm GitHub release binaries and `SHA256SUMS` are present and match the
  built artifacts.
- [ ] Confirm registry publication succeeded for every package included in the
  release. Check all Node platform packages as well as the root package.
- [ ] Install the CLI from the release instructions on supported operating
  systems and verify its reported version.
- [ ] Install the published Python and Node packages in clean environments and
  verify imports, CLI availability, and package versions.
- [ ] Verify the Rust crate can be installed from crates.io when it is part of
  the release.
- [ ] Run a short smoke test for the released default transport and the
  documented setup path.
- [ ] Confirm the website has deployed the release notes and updated guides.
- [ ] Record the tag, commit, workflow runs, registry versions, and any
  exceptions in the release record.

## If a release job fails

- [ ] Identify whether the version was published to any registry before
  retrying.
- [ ] For an unpublished artifact, fix the workflow or package issue and rerun
  the job for the same tag after checking its version guard.
- [ ] For an already published artifact, do not try to overwrite that version.
  Verify its contents, complete other registries only when they can use the
  same source, or prepare a new patch release if the artifact is wrong.
- [ ] Do not move or recreate a release tag to point at different source.
- [ ] Update the release record with the partial publication and recovery
  outcome.
