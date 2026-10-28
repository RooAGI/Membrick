---
title: Experimental Wasm worker
description: Bounded pure-computation worker and its current security boundary.
---

# Experimental Wasm worker

Membrick includes an opt-in runtime crate at
`crates/membrick-wasm-extension-runtime`. It runs core Wasm modules in a
separate worker process using Wasmi. It is a pure-computation foundation. It
does not implement `ExtensionHandler`, register tools or routes, or call
Membrick capabilities. The current `ExtensionRegistry` continues to dispatch
trusted statically linked native code.

## Guest contract

The module is a core Wasm binary compiled for `wasm32-unknown-unknown`. It must
declare no imports. The worker does not create a WASI context or linker, so
filesystem, network, environment, process, and host-call access are absent
from the guest ABI. Unknown imports, including WASI imports, are rejected.

The module exports `memory`, `membrick_alloc(i32) -> i32`, and
`membrick_invoke(i32, i32) -> i64`. The host writes one JSON request into the
guest's memory. The result packs its pointer in the upper 32 bits and its byte
length in the lower 32 bits. Output must also be JSON. No host identity,
storage handle, permission grant, or callback is exposed to the guest.

## Limits and failure behavior

`RuntimeLimits` bounds module and JSON payload sizes, linear memory, Wasm fuel,
wall-clock duration, and simultaneous workers. Saturated calls fail with
`Busy`; the runner does not build an unbounded waiter queue. The parent polls
the worker and kills and reaps it when the wall-clock limit expires. Traps and
protocol failures become coarse errors without guest data. A worker crash
affects one call; the next call starts a fresh process.

The defaults are conservative starting values, not universal safe settings.
The runner validates configured values against hard maxima. These Wasmi
limits bound guest execution; they do not impose an OS memory or CPU limit on
the worker process itself.

## OS sandbox status

The standalone runner still accepts a host-configured `SandboxCommand`, but
that command is explicitly unverified and is not an activation boundary.
`WasmExtensionRunner::new_bubblewrap` uses a Membrick-owned fixed profile on
Linux. It requires the root-owned `/usr/bin/bwrap`, user, mount, PID, IPC, UTS
and network namespaces, a private read-only root, a 16 MiB writable `/tmp`,
closed inherited non-stdio descriptors, worker seccomp installed before
request parsing, and inherited CPU and address-space limits. It fails closed
when a startup probe cannot demonstrate those properties.

The profile mounts only the trusted worker and the exact shared-library files
reported by the host's fixed `/usr/bin/ldd`; it does not expose whole host
library directories. The loader cache and host configuration directories are
not mounted. The worker's required dynamic loader and libraries remain
readable inside the process, so their bytes are visible to that worker.
Bubblewrap is only the namespace and filesystem construction tool. Security
depends on Membrick's fixed arguments, the worker filter, resource limits, and
the startup probe, not on Bubblewrap alone.

On macOS, the available `sandbox-exec` tool is deprecated; Windows and other
targets also have no supported Membrick-owned backend. `new_bubblewrap` returns
`UnsupportedPlatform` there. Missing Linux prerequisites cause runner
construction to fail; no weaker fallback is used. The host activation gate
remains disabled because the generic registry bridge, capability RPC, and
extension-scoped authorization are not implemented. This backend does not
make third-party extension activation available.

The worker path is host-owned configuration and must point to a trusted,
installed executable. Do not derive it from an extension manifest, package, or
registry. Extension artifact bytes are sent to the worker through a bounded
private pipe; the guest itself is never native-loaded into Membrick. The
Linux workers retain the host's UID/GID identity. Namespaces, filesystem,
seccomp, and resource limits reduce available rights; a kernel or Bubblewrap
vulnerability remains a host security risk.

## Runtime patch policy

The standalone runtime crate pins Wasmi to an exact version and carries its own
lockfile. Build and test it with `cargo build --locked` and `cargo test
--locked` from the crate directory. A Wasmi update is a security-sensitive
change: review upstream release notes and advisories, refresh the lockfile,
run all runtime tests, review enabled Wasm proposals and limits, and repeat the
worker sandbox review before shipping. Apply urgent upstream security fixes
promptly, then validate the updated version before deployment. Do not use an
unlocked or floating runtime in packaged builds.

## What remains before extension activation

The trust verifier and admission layer recognize `wasm-core-module` with
target `wasm32-unknown-unknown`; this intentionally differs from a Wasm
component and `wasm32-wasip2`. Admission does not load or execute the bytes.
`activate_admitted_extension` consumes an admitted artifact and returns a
typed fail-closed error listing the current blockers. The worker is not
connected to `ExtensionRegistry` or `ExtensionContext`. The registry stores
only static descriptor and handler references; a bridge needs owned runtime
descriptors and handlers rather than leaked static values. The worker's
no-import ABI also cannot call capabilities. Any future capability bridge
must perform per-hop registry grant checks and apply the admitted effective
grants. Extension-specific document authorization must be tied to those grants
as well. Until these API pieces are implemented and reviewed,
third-party Wasm activation is unavailable. Treat static `linkme` extensions
as trusted executable code only.
