//! Experimental bounded process adapter for core Wasm computation.
//!
//! The worker accepts a deliberately small JSON ABI and defines no imports.
//! It is an execution foundation, not an [`ExtensionHandler`] loader or a
//! bridge to Membrick capabilities. Its launcher is not verified to enforce
//! OS isolation, so this crate must not be used as an untrusted extension
//! activation path.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux_bubblewrap;

const REQUEST_HEADER_BYTES: usize = 16;
const RESPONSE_HEADER_BYTES: usize = 5;
const MAX_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_MEMORY: usize = 512 * 1024 * 1024;
const MAX_FUEL: u64 = 10_000_000_000;
const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENCY: usize = 64;

/// Explicit resource ceilings for one runner.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub memory_bytes: usize,
    pub fuel: u64,
    pub timeout: Duration,
    pub module_bytes: usize,
    pub payload_bytes: usize,
    pub max_concurrency: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            fuel: 50_000_000,
            timeout: Duration::from_secs(5),
            module_bytes: 8 * 1024 * 1024,
            payload_bytes: 1024 * 1024,
            max_concurrency: 4,
        }
    }
}

impl RuntimeLimits {
    fn validate(&self) -> Result<(), RuntimeError> {
        if self.memory_bytes == 0
            || self.memory_bytes > MAX_MEMORY
            || self.fuel == 0
            || self.fuel > MAX_FUEL
            || self.timeout.is_zero()
            || self.timeout > MAX_TIMEOUT
            || self.module_bytes == 0
            || self.module_bytes > MAX_MODULE_BYTES
            || self.payload_bytes == 0
            || self.payload_bytes > MAX_PAYLOAD_BYTES
            || self.max_concurrency == 0
            || self.max_concurrency > MAX_CONCURRENCY
        {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(())
    }
}

/// Runtime failures are intentionally coarse and contain no guest payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidLimits,
    ModuleTooLarge,
    PayloadTooLarge,
    InvalidJson,
    WorkerUnavailable,
    SandboxRequired,
    UnsupportedPlatform,
    SandboxUnavailable,
    SandboxProbeFailed,
    Busy,
    TimedOut,
    WorkerFailed,
    GuestRejected,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidLimits => "invalid Wasm runtime limits",
            Self::ModuleTooLarge => "Wasm module exceeds configured byte limit",
            Self::PayloadTooLarge => "Wasm payload exceeds configured byte limit",
            Self::InvalidJson => "Wasm payload is not valid JSON",
            Self::WorkerUnavailable => "Wasm worker could not be started",
            Self::SandboxRequired => {
                "Wasm worker launcher command is empty; OS isolation is not verified"
            }
            Self::UnsupportedPlatform => "Wasm OS sandbox is unavailable on this platform",
            Self::SandboxUnavailable => "required Linux Bubblewrap sandbox is unavailable",
            Self::SandboxProbeFailed => "Linux Bubblewrap security probe failed",
            Self::Busy => "Wasm runner concurrency limit is reached",
            Self::TimedOut => "Wasm worker exceeded its wall-clock limit",
            Self::WorkerFailed => "Wasm worker failed",
            Self::GuestRejected => "Wasm guest rejected the invocation",
        })
    }
}

impl std::error::Error for RuntimeError {}

/// Process-backed runner. The executable is a trusted, host-installed worker.
pub struct WasmExtensionRunner {
    worker: PathBuf,
    launcher: WorkerLauncher,
    limits: RuntimeLimits,
    active: Arc<Mutex<usize>>,
}

enum WorkerLauncher {
    Unverified(SandboxCommand),
    #[cfg(target_os = "linux")]
    Bubblewrap(linux_bubblewrap::BubblewrapSandbox),
}

/// Raw host-configured launcher command. It receives the worker path and
/// `--membrick-wasm-worker` after these fixed arguments. The runtime does not
/// verify that this command enforces any OS policy; it is not a sandbox
/// profile and is not used by Membrick's extension activation boundary.
#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
}

impl WasmExtensionRunner {
    pub fn new(
        worker: impl Into<PathBuf>,
        sandbox: SandboxCommand,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        if sandbox.program.as_os_str().is_empty() {
            return Err(RuntimeError::SandboxRequired);
        }
        limits.validate()?;
        Ok(Self {
            worker: worker.into(),
            launcher: WorkerLauncher::Unverified(sandbox),
            active: Arc::new(Mutex::new(0)),
            limits,
        })
    }

    /// Create a runner with Membrick's fixed Linux Bubblewrap profile.
    ///
    /// The worker path must refer to a trusted, host-installed worker binary.
    /// This constructor performs a sandbox probe and returns an error if
    /// Bubblewrap or any required kernel isolation feature is unavailable.
    /// It does not enable extension registry dispatch or capability access.
    #[cfg(target_os = "linux")]
    pub fn new_bubblewrap(
        worker: impl Into<PathBuf>,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        limits.validate()?;
        let sandbox = linux_bubblewrap::BubblewrapSandbox::new(worker.into(), &limits)?;
        Ok(Self {
            worker: sandbox.worker_path().to_owned(),
            launcher: WorkerLauncher::Bubblewrap(sandbox),
            active: Arc::new(Mutex::new(0)),
            limits,
        })
    }

    /// This target has no supported Membrick-owned OS sandbox backend.
    #[cfg(not(target_os = "linux"))]
    pub fn new_bubblewrap(
        _worker: impl Into<PathBuf>,
        _limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform)
    }

    /// Invoke a module using `membrick_alloc(i32) -> i32` and
    /// `membrick_invoke(i32, i32) -> i64`. The returned i64 packs the output
    /// pointer in its high 32 bits and output length in its low 32 bits.
    /// Modules must import no functions, memories, tables, or globals.
    pub fn invoke(&self, module: &[u8], input_json: &[u8]) -> Result<Vec<u8>, RuntimeError> {
        if module.len() > self.limits.module_bytes {
            return Err(RuntimeError::ModuleTooLarge);
        }
        if input_json.len() > self.limits.payload_bytes {
            return Err(RuntimeError::PayloadTooLarge);
        }
        let _slot = self.acquire_slot()?;
        serde_json::from_slice::<serde_json::Value>(input_json)
            .map_err(|_| RuntimeError::InvalidJson)?;

        let mut command = match &self.launcher {
            WorkerLauncher::Unverified(sandbox) => {
                let mut command = Command::new(&sandbox.program);
                command
                    .args(&sandbox.arguments)
                    .arg(&self.worker)
                    .arg("--membrick-wasm-worker");
                command
            }
            #[cfg(target_os = "linux")]
            WorkerLauncher::Bubblewrap(sandbox) => sandbox.command(&self.limits)?,
        };
        let child = command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| RuntimeError::WorkerUnavailable)?;
        let mut guard = ChildGuard(Some(child));
        let child = guard.0.as_mut().expect("child guard populated");
        let mut stdin = child.stdin.take().ok_or(RuntimeError::WorkerFailed)?;
        let mut stdout = child.stdout.take().ok_or(RuntimeError::WorkerFailed)?;
        let module_bytes = module.to_vec();
        let input_bytes = input_json.to_vec();
        let memory_limit = self.limits.memory_bytes as u64;
        let fuel_limit = self.limits.fuel;
        let payload_limit = self.limits.payload_bytes as u64;
        let writer = thread::spawn(move || {
            stdin.write_all(&(module_bytes.len() as u64).to_le_bytes())?;
            stdin.write_all(&(input_bytes.len() as u64).to_le_bytes())?;
            stdin.write_all(&memory_limit.to_le_bytes())?;
            stdin.write_all(&fuel_limit.to_le_bytes())?;
            stdin.write_all(&payload_limit.to_le_bytes())?;
            stdin.write_all(&module_bytes)?;
            stdin.write_all(&input_bytes)?;
            stdin.flush()
        });
        let response_limit = self
            .limits
            .payload_bytes
            .saturating_add(RESPONSE_HEADER_BYTES);
        let reader = thread::spawn(move || read_bounded(&mut stdout, response_limit));
        let deadline = Instant::now() + self.limits.timeout;
        loop {
            if let Some(status) = child.try_wait().map_err(|_| RuntimeError::WorkerFailed)? {
                let _ = writer.join();
                let response = reader
                    .join()
                    .map_err(|_| RuntimeError::WorkerFailed)?
                    .map_err(|_| RuntimeError::WorkerFailed)?;
                guard.0.take();
                if !status.success() || response.len() < RESPONSE_HEADER_BYTES {
                    return Err(RuntimeError::WorkerFailed);
                }
                let allowed =
                    u32::from_le_bytes(response[1..5].try_into().expect("header")) as usize;
                if response[0] != 0
                    || allowed > self.limits.payload_bytes
                    || response.len() != 5 + allowed
                {
                    return Err(RuntimeError::GuestRejected);
                }
                let body = response[5..].to_vec();
                serde_json::from_slice::<serde_json::Value>(&body)
                    .map_err(|_| RuntimeError::WorkerFailed)?;
                return Ok(body);
            }
            if Instant::now() >= deadline {
                if let Some(mut child) = guard.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = writer.join();
                let _ = reader.join();
                return Err(RuntimeError::TimedOut);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A saturated runner rejects calls rather than accumulating waiters.
    fn acquire_slot(&self) -> Result<SlotGuard, RuntimeError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *active >= self.limits.max_concurrency {
            return Err(RuntimeError::Busy);
        }
        *active += 1;
        Ok(SlotGuard(Arc::clone(&self.active)))
    }
}

struct SlotGuard(Arc<Mutex<usize>>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut used = self.0.lock().unwrap_or_else(|error| error.into_inner());
        *used = used.saturating_sub(1);
    }
}

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "worker output too large",
        ));
    }
    Ok(bytes)
}

/// Entry point used only by the trusted worker executable.
pub fn run_worker() -> Result<(), RuntimeError> {
    use wasmi::{Config, Engine, Linker, Module, Store, StoreLimitsBuilder};

    #[cfg(target_os = "linux")]
    linux_bubblewrap::install_worker_seccomp()?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut header = [0_u8; REQUEST_HEADER_BYTES + 24];
    input
        .read_exact(&mut header)
        .map_err(|_| RuntimeError::WorkerFailed)?;
    let module_len = u64::from_le_bytes(header[0..8].try_into().expect("field")) as usize;
    let payload_len = u64::from_le_bytes(header[8..16].try_into().expect("field")) as usize;
    let memory_limit = u64::from_le_bytes(header[16..24].try_into().expect("field")) as usize;
    let fuel = u64::from_le_bytes(header[24..32].try_into().expect("field"));
    let payload_limit = u64::from_le_bytes(header[32..40].try_into().expect("field")) as usize;
    if module_len > MAX_MODULE_BYTES
        || payload_len > MAX_PAYLOAD_BYTES
        || payload_len > payload_limit
    {
        return send_worker_response(1, b"request limit exceeded");
    }
    if memory_limit == 0 || memory_limit > MAX_MEMORY || fuel == 0 || fuel > MAX_FUEL {
        return send_worker_response(1, b"invalid worker limits");
    }
    let mut module_bytes = vec![0; module_len];
    let mut payload = vec![0; payload_len];
    input
        .read_exact(&mut module_bytes)
        .map_err(|_| RuntimeError::WorkerFailed)?;
    input
        .read_exact(&mut payload)
        .map_err(|_| RuntimeError::WorkerFailed)?;

    let result = (|| -> Result<Vec<u8>, RuntimeError> {
        serde_json::from_slice::<serde_json::Value>(&payload)
            .map_err(|_| RuntimeError::InvalidJson)?;
        let mut config = Config::default();
        config.consume_fuel(true).allow_start_fn(false);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, &module_bytes[..]).map_err(|_| RuntimeError::GuestRejected)?;
        if module.imports().next().is_some() {
            return Err(RuntimeError::GuestRejected);
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_limit)
            .table_elements(10_000)
            .instances(1)
            .tables(4)
            .memories(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&engine, limits);
        store.limiter(|limits| limits);
        store
            .set_fuel(fuel)
            .map_err(|_| RuntimeError::WorkerFailed)?;
        let linker = Linker::<wasmi::StoreLimits>::new(&engine);
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|_| RuntimeError::GuestRejected)?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(RuntimeError::GuestRejected)?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "membrick_alloc")
            .map_err(|_| RuntimeError::GuestRejected)?;
        let invoke = instance
            .get_typed_func::<(i32, i32), i64>(&store, "membrick_invoke")
            .map_err(|_| RuntimeError::GuestRejected)?;
        let input_len = i32::try_from(payload.len()).map_err(|_| RuntimeError::PayloadTooLarge)?;
        let input_ptr = alloc
            .call(&mut store, input_len)
            .map_err(|_| RuntimeError::GuestRejected)?;
        if input_ptr < 0 {
            return Err(RuntimeError::GuestRejected);
        }
        memory
            .write(&mut store, input_ptr as usize, &payload)
            .map_err(|_| RuntimeError::GuestRejected)?;
        let packed = invoke
            .call(&mut store, (input_ptr, input_len))
            .map_err(|_| RuntimeError::GuestRejected)? as u64;
        let output_ptr = (packed >> 32) as usize;
        let output_len = packed as u32 as usize;
        if output_len > payload_limit {
            return Err(RuntimeError::PayloadTooLarge);
        }
        let mut output = vec![0; output_len];
        memory
            .read(&store, output_ptr, &mut output)
            .map_err(|_| RuntimeError::GuestRejected)?;
        serde_json::from_slice::<serde_json::Value>(&output)
            .map_err(|_| RuntimeError::GuestRejected)?;
        Ok(output)
    })();
    match result {
        Ok(output) => send_worker_response(0, &output),
        Err(_) => send_worker_response(1, b"guest invocation failed"),
    }
}

/// Emit the fixed OS-isolation evidence used by `new_bubblewrap` at startup.
/// This is a diagnostic mode in the trusted worker, not an extension ABI.
pub fn run_sandbox_probe(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<(), RuntimeError> {
    #[cfg(target_os = "linux")]
    {
        linux_bubblewrap::install_worker_seccomp()?;
        let args = args.into_iter().collect::<Vec<_>>();
        if args.len() != 3 {
            return Err(RuntimeError::SandboxProbeFailed);
        }
        let report = linux_bubblewrap::worker_sandbox_probe(&args[0], &args[1], &args[2])?;
        std::io::stdout()
            .write_all(&report)
            .map_err(|_| RuntimeError::SandboxProbeFailed)?;
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        Err(RuntimeError::UnsupportedPlatform)
    }
}

fn send_worker_response(status: u8, body: &[u8]) -> Result<(), RuntimeError> {
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(RuntimeError::PayloadTooLarge);
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    output
        .write_all(&[status])
        .map_err(|_| RuntimeError::WorkerFailed)?;
    output
        .write_all(&(body.len() as u32).to_le_bytes())
        .map_err(|_| RuntimeError::WorkerFailed)?;
    output
        .write_all(body)
        .map_err(|_| RuntimeError::WorkerFailed)?;
    output.flush().map_err(|_| RuntimeError::WorkerFailed)
}
