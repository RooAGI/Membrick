---
title: Extension security
description: How Membrick accepts and trusts compiled-in extensions.
---

# Extension security

## Current approach

Membrick accepts extensions as reviewed source code linked into an official
host build. A contributor proposes changes through the normal code review
process. Maintainers review the implementation and dependencies, CI checks the
change, and an approved change ships with a Membrick release.

There is no third-party extension download, installation, or activation path.
The host does not execute extension code supplied by end users.

```mermaid
flowchart LR
    C[Contributor] -->|pull request| R[Review and CI]
    R -->|approved code| B[Official Membrick build]
    B -->|built-in extension| H[Membrick host]
```

## What this protects

Review and CI reduce the chance of shipping accidental defects or unreviewed
code. They do not prove code is harmless. A compiled-in Rust extension runs in
the Membrick process with the host's operating-system permissions. Extension
storage APIs scope normal data access to the current owner and workspace, but
they are not an isolation boundary against malicious native code.

Only include extensions from contributors and maintainers you trust. Review
new dependencies and upgrades, keep dependency versions pinned by the lockfile,
and ship extensions through the same release process as Membrick itself.

## Hashes and signatures

A hash identifies exact bytes. A signature can show that a trusted key
approved those bytes. Neither shows that the code is safe. Because current
extensions are linked into Membrick, users receive them as part of the host
build rather than as separately installed extension artifacts.

## If independent third-party installation is needed later

Do not load an unknown native library into the Membrick process. A future
independent-install model would need a separate process, a narrow host API, and
operating-system-enforced limits on filesystem, network, and resource access.
That is a separate design and is not required for the current reviewed,
compiled-in model.
