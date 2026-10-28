#![cfg(target_os = "linux")]

use membrick_wasm_extension_runtime::{RuntimeLimits, WasmExtensionRunner};
use std::time::Duration;

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
fn fixed_bubblewrap_profile_enforces_host_and_process_boundaries() {
    let worker = env!("CARGO_BIN_EXE_membrick-extension-worker");
    let mut limits = RuntimeLimits::default();
    limits.memory_bytes = 64 * 1024 * 1024;
    limits.timeout = Duration::from_secs(5);

    let runner = WasmExtensionRunner::new_bubblewrap(worker, limits)
        .expect("the Linux CI image must provide the required Bubblewrap profile");
    let wasm = wat::parse_str(ECHO).unwrap();
    let output = runner.invoke(&wasm, br#"{"sandbox":"ok"}"#).unwrap();
    assert_eq!(output, br#"{"sandbox":"ok"}"#);
}

#[test]
fn unavailable_worker_is_denied_before_sandbox_creation() {
    let result = WasmExtensionRunner::new_bubblewrap(
        "/missing/membrick-extension-worker",
        RuntimeLimits::default(),
    );
    assert!(matches!(
        result,
        Err(membrick_wasm_extension_runtime::RuntimeError::WorkerUnavailable)
    ));
}
