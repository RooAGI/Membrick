use membrick_wasm_extension_runtime::{
    RuntimeError, RuntimeLimits, SandboxCommand, WasmExtensionRunner,
};
use std::ffi::OsString;
use std::time::Duration;

fn runner(worker: &str, limits: RuntimeLimits) -> WasmExtensionRunner {
    // Test shim only. Production must supply an OS sandbox launcher.
    WasmExtensionRunner::new(
        worker,
        SandboxCommand {
            program: "/usr/bin/env".into(),
            arguments: Vec::<OsString>::new(),
        },
        limits,
    )
    .unwrap()
}

fn real_runner(limits: RuntimeLimits) -> WasmExtensionRunner {
    runner(env!("CARGO_BIN_EXE_membrick-extension-worker"), limits)
}

fn module(source: &str) -> Vec<u8> {
    wat::parse_str(source).unwrap()
}

const ECHO: &str = r#"(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "membrick_alloc") (param $n i32) (result i32)
    (local $p i32) (global.get $heap) (local.set $p)
    (global.get $heap) (local.get $n) (i32.add) (global.set $heap)
    (local.get $p))
  (func (export "membrick_invoke") (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n)))))"#;

#[test]
fn worker_runs_no_import_json_module() {
    let result = real_runner(RuntimeLimits::default())
        .invoke(&module(ECHO), br#"{"ok":true}"#)
        .unwrap();
    assert_eq!(result, br#"{"ok":true}"#);
}

#[test]
fn unknown_host_import_is_denied() {
    let wasm = module(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)))"#);
    assert_eq!(
        real_runner(RuntimeLimits::default()).invoke(&wasm, b"{}"),
        Err(RuntimeError::GuestRejected)
    );
}

#[test]
fn invalid_abi_and_memory_limit_are_denied() {
    let no_abi = module(r#"(module (memory (export "memory") 1))"#);
    assert_eq!(
        real_runner(RuntimeLimits::default()).invoke(&no_abi, b"{}"),
        Err(RuntimeError::GuestRejected)
    );
    let too_large = module(
        r#"(module (memory (export "memory") 2 2)
      (func (export "membrick_alloc") (param i32) (result i32) i32.const 0)
      (func (export "membrick_invoke") (param i32 i32) (result i64) i64.const 0))"#,
    );
    let mut limits = RuntimeLimits::default();
    limits.memory_bytes = 64 * 1024;
    assert_eq!(
        real_runner(limits).invoke(&too_large, b"{}"),
        Err(RuntimeError::GuestRejected)
    );
}

#[test]
fn table_limit_is_enforced() {
    let too_many = module(r#"(module (table 10001 funcref))"#);
    assert_eq!(
        real_runner(RuntimeLimits::default()).invoke(&too_many, b"{}"),
        Err(RuntimeError::GuestRejected)
    );
}

#[test]
fn missing_sandbox_fails_closed_at_construction() {
    let result = WasmExtensionRunner::new(
        env!("CARGO_BIN_EXE_membrick-extension-worker"),
        SandboxCommand {
            program: "".into(),
            arguments: Vec::<OsString>::new(),
        },
        RuntimeLimits::default(),
    );
    assert!(matches!(result, Err(RuntimeError::SandboxRequired)));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn bubblewrap_backend_is_unavailable_off_linux() {
    assert!(matches!(
        WasmExtensionRunner::new_bubblewrap(
            env!("CARGO_BIN_EXE_membrick-extension-worker"),
            RuntimeLimits::default()
        ),
        Err(RuntimeError::UnsupportedPlatform)
    ));
}

#[test]
fn fuel_exhaustion_is_contained() {
    let wasm = module(
        r#"(module (memory (export "memory") 1)
      (func (export "membrick_alloc") (param i32) (result i32) i32.const 0)
      (func (export "membrick_invoke") (param i32 i32) (result i64)
        (loop $forever br $forever) (i64.const 0)))"#,
    );
    let mut limits = RuntimeLimits::default();
    limits.fuel = 100;
    assert_eq!(
        real_runner(limits).invoke(&wasm, b"{}"),
        Err(RuntimeError::GuestRejected)
    );
}

#[test]
fn module_and_payload_limits_fail_before_worker_start() {
    let mut limits = RuntimeLimits::default();
    limits.module_bytes = 1;
    assert_eq!(
        real_runner(limits.clone()).invoke(b"too long", b"{}"),
        Err(RuntimeError::ModuleTooLarge)
    );
    limits.module_bytes = 4096;
    limits.payload_bytes = 2;
    assert_eq!(
        real_runner(limits.clone()).invoke(&module(ECHO), b"{} "),
        Err(RuntimeError::PayloadTooLarge)
    );
    limits.payload_bytes = 4096;
    assert_eq!(
        real_runner(limits).invoke(&module(ECHO), b"not json"),
        Err(RuntimeError::InvalidJson)
    );
}

#[test]
fn timeout_kills_and_reaps_worker() {
    let wasm = module(
        r#"(module (memory (export "memory") 1)
      (func (export "membrick_alloc") (param i32) (result i32) i32.const 0)
      (func (export "membrick_invoke") (param i32 i32) (result i64)
        (loop $forever br $forever) (i64.const 0)))"#,
    );
    let mut limits = RuntimeLimits::default();
    limits.timeout = Duration::from_millis(100);
    limits.fuel = 10_000_000_000;
    assert_eq!(
        real_runner(limits).invoke(&wasm, b"{}"),
        Err(RuntimeError::TimedOut)
    );
    assert_eq!(
        real_runner(RuntimeLimits::default())
            .invoke(&module(ECHO), b"{}")
            .unwrap(),
        b"{}"
    );
}

#[cfg(unix)]
#[test]
fn crashed_worker_returns_error_without_affecting_next_runner() {
    use std::os::unix::fs::PermissionsExt;
    let temp = std::env::temp_dir().join(format!("membrick-worker-crash-{}", std::process::id()));
    std::fs::write(&temp, "#!/bin/sh\nexit 17\n").unwrap();
    let mut permissions = std::fs::metadata(&temp).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&temp, permissions).unwrap();
    assert_eq!(
        runner(temp.to_str().unwrap(), RuntimeLimits::default()).invoke(&module(ECHO), b"{}"),
        Err(RuntimeError::WorkerFailed)
    );
    std::fs::remove_file(temp).unwrap();
    assert_eq!(
        real_runner(RuntimeLimits::default())
            .invoke(&module(ECHO), b"{}")
            .unwrap(),
        b"{}"
    );
}

#[cfg(unix)]
#[test]
fn truncated_worker_response_is_denied() {
    use std::os::unix::fs::PermissionsExt;
    let temp = std::env::temp_dir().join(format!("membrick-worker-empty-{}", std::process::id()));
    std::fs::write(&temp, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&temp).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&temp, permissions).unwrap();
    assert_eq!(
        runner(temp.to_str().unwrap(), RuntimeLimits::default()).invoke(&module(ECHO), b"{}"),
        Err(RuntimeError::WorkerFailed)
    );
    std::fs::remove_file(temp).unwrap();
}

#[test]
fn concurrency_limit_rejects_instead_of_queueing() {
    let mut limits = RuntimeLimits::default();
    limits.max_concurrency = 1;
    limits.timeout = Duration::from_millis(500);
    limits.fuel = 10_000_000_000;
    let runner = std::sync::Arc::new(real_runner(limits));
    let wasm = module(
        r#"(module (memory (export "memory") 1)
      (func (export "membrick_alloc") (param i32) (result i32) i32.const 0)
      (func (export "membrick_invoke") (param i32 i32) (result i64)
        (loop $forever br $forever) (i64.const 0)))"#,
    );
    let first_runner = std::sync::Arc::clone(&runner);
    let first_module = wasm.clone();
    let first = std::thread::spawn(move || first_runner.invoke(&first_module, b"{}"));
    std::thread::sleep(Duration::from_millis(75));
    assert_eq!(runner.invoke(&wasm, b"{}"), Err(RuntimeError::Busy));
    assert!(matches!(
        first.join().unwrap(),
        Err(RuntimeError::TimedOut) | Err(RuntimeError::WorkerFailed)
    ));
}
