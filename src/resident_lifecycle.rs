use super::*;

/// A crash-safe lease held by a request or active provider session. The
/// resident stays up while any request or session owns a lease.
pub(crate) struct ProviderSessionLease {
    file: Option<fs::File>,
    path: PathBuf,
}

impl ProviderSessionLease {
    pub(crate) fn acquire(root: &Path, provider: &str) -> Result<Self> {
        static NEXT_LEASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let lifecycle_gate = open_lifecycle_gate(root)?;
        lifecycle_gate
            .lock()
            .context("could not acquire project provider lifecycle lock")?;
        let directory = runtime_dir(root)?.join("provider-leases");
        fs::create_dir_all(&directory)?;
        let nonce = NEXT_LEASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stem = format!(
            "{}-{}-{nonce}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let starting = directory.join(format!("{stem}.starting"));
        let path = directory.join(format!("{stem}.lease"));
        let marker = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&starting)?;
        let registration = (|| -> Result<fs::File> {
            marker
                .try_lock()
                .with_context(|| format!("could not lock provider lease {}", starting.display()))?;
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.try_lock()
                .with_context(|| format!("could not lock provider lease {}", path.display()))?;
            writeln!(file, "provider={provider}")?;
            Ok(file)
        })();
        let file = match registration {
            Ok(file) => file,
            Err(error) => {
                drop(marker);
                let _ = fs::remove_file(&starting);
                return Err(error);
            }
        };
        // The locked lease now protects the client while we release and remove
        // the temporary marker. This also avoids deleting an open file on
        // platforms that restrict that operation.
        drop(marker);
        fs::remove_file(&starting)?;
        drop(lifecycle_gate);
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

/// Keep a resident warm for the lifetime of a bound provider session.
pub(crate) fn acquire_provider_session_lease(
    root: &Path,
    provider: &str,
) -> Result<ProviderSessionLease> {
    ProviderSessionLease::acquire(root, provider)
}

impl Drop for ProviderSessionLease {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

/// Warm the resident when an MCP adapter starts. This request owns only a
/// temporary lease; adapter process lifetime does not pin the resident open.
pub(crate) fn ensure_provider_resident(root: &Path, provider: &str) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    tracing::debug!(provider, "provider requested resident startup");
    let startup_root = root.clone();
    // Provider MCP servers are CLI modes. Capture the CLI executable in this
    // shared lifecycle layer so provider adapters do not choose their own
    // resident process image.
    let resident_executable =
        std::env::current_exe().context("could not locate Membrick CLI for resident startup")?;
    std::thread::Builder::new()
        .name("membrick-resident-startup".into())
        .spawn(move || {
            if let Err(error) = request_with_transport_safe(
                &startup_root,
                hyper::Method::GET,
                "/health",
                None,
                false,
                Some(&resident_executable),
            ) {
                tracing::warn!("provider resident startup failed: {error}");
            }
        })?;
    Ok(())
}

/// Completes when the resident should stop: on `signal`, or for a managed
/// resident once no client request holds a lease through the idle timeout. Returns the lifecycle
/// lock when one is held, so no new client registers until this resident has
/// flushed and exited.
pub(crate) async fn resident_shutdown(
    root: &Path,
    managed: bool,
    idle_timeout: Duration,
    signal: impl std::future::Future<Output = ()>,
) -> Option<fs::File> {
    if managed {
        tokio::select! {
            _ = signal => {}
            gate = wait_for_no_provider_sessions(root, idle_timeout) => {
                tracing::info!(idle_timeout_seconds = idle_timeout.as_secs(), "resident idle timeout elapsed; shutting down");
                return Some(gate);
            }
        }
    } else {
        signal.await;
    }
    tracing::info!("shutdown signal received; shutting down");
    // A client may be registering right now; wait for it, then hold the lock.
    let root = root.to_owned();
    let gate = tokio::task::spawn_blocking(move || {
        let gate = open_lifecycle_gate(&root)?;
        gate.lock()?;
        anyhow::Ok(gate)
    })
    .await;
    match gate {
        Ok(Ok(gate)) => Some(gate),
        Ok(Err(error)) => {
            tracing::warn!("could not hold provider lifecycle lock during shutdown: {error:#}");
            None
        }
        Err(error) => {
            tracing::warn!("could not hold provider lifecycle lock during shutdown: {error}");
            None
        }
    }
}

/// Wait until every client request and provider session has released its
/// lease for the idle timeout. Crashed clients release the OS lock; leftover
/// lease files are removed here.
pub(crate) async fn wait_for_no_provider_sessions(root: &Path, grace: Duration) -> fs::File {
    let directory = root.join(".membrick/runtime/provider-leases");
    let mut empty_since = None;
    loop {
        match provider_session_is_active(&directory) {
            Ok(true) => empty_since = None,
            Ok(false) => {
                let since = empty_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= grace {
                    match open_lifecycle_gate(root) {
                        Ok(gate) => match gate.try_lock() {
                            Ok(()) => match remove_released_provider_leases(&directory)
                                .and_then(|()| provider_session_is_active(&directory))
                            {
                                Ok(false) => return gate,
                                Ok(true) => empty_since = None,
                                Err(error) => {
                                    tracing::warn!("provider lease recheck failed; keeping resident alive: {error:#}");
                                    empty_since = None;
                                }
                            },
                            Err(fs::TryLockError::WouldBlock) => empty_since = None,
                            Err(fs::TryLockError::Error(error)) => {
                                tracing::warn!("provider lifecycle lock failed; keeping resident alive: {error}");
                                empty_since = None;
                            }
                        },
                        Err(error) => {
                            tracing::warn!("provider lifecycle lock unavailable; keeping resident alive: {error:#}");
                            empty_since = None;
                        }
                    }
                }
            }
            Err(error) => {
                // A scan error must never stop a resident while a provider
                // may still be connected.
                tracing::warn!("provider lease scan failed; keeping resident alive: {error:#}");
                empty_since = None;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(crate) fn open_lifecycle_gate(root: &Path) -> Result<fs::File> {
    let path = runtime_dir(root)?.join("provider-lifecycle.lock");
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("could not open provider lifecycle lock {}", path.display()))
}

/// Whether any client holds a kernel-locked lease or registration marker.
/// This never deletes: a file can be unlocked only because its owner is
/// between creating and locking it.
pub(crate) fn provider_session_is_active(directory: &Path) -> Result<bool> {
    for path in provider_lease_files(directory)? {
        // A file removed by its owner since the listing is simply gone.
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        match file.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => return Ok(true),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Ok(()) => {}
        }
    }
    Ok(false)
}

/// Remove lease files left by crashed clients. The caller must hold the
/// lifecycle lock, which every client holds while it creates and locks its
/// files, so an unlocked file here can only be stale.
pub(crate) fn remove_released_provider_leases(directory: &Path) -> Result<()> {
    for path in provider_lease_files(directory)? {
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        if file.try_lock().is_ok() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

pub(crate) fn provider_lease_files(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    Ok(fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("lease" | "starting")
            )
        })
        .collect())
}

pub(crate) fn runtime_dir(root: &Path) -> Result<PathBuf> {
    let runtime = root.join(".membrick").join("runtime");
    fs::create_dir_all(&runtime).with_context(|| {
        format!(
            "could not create project runtime directory {}",
            runtime.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    }
    Ok(runtime)
}

/// A stable per-user, per-project name for the files that coordinate startup.
pub(crate) fn instance_name(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let identity =
        std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(identity.as_bytes());
    hash.update([0]);
    hash.update(root.as_os_str().as_encoded_bytes());
    let digest = hash.finalize();
    format!(
        "mb-{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// How clients reach the running resident. Like uWSGI, each resident binds a
/// socket with a fresh name and records it in the project once it listens;
/// clients read the record instead of deriving a path.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResidentRecord {
    pub(crate) pid: u32,
    pub(crate) socket: PathBuf,
}

/// Where the record lives. Reading or removing it never creates the runtime
/// directory, so a client or a resident shutting down cannot recreate a
/// project folder that is being deleted.
pub(crate) fn resident_record_path(root: &Path) -> PathBuf {
    root.join(".membrick").join("runtime").join("resident.json")
}

pub(crate) fn read_resident_record(root: &Path) -> Result<ResidentRecord> {
    let path = resident_record_path(root);
    let bytes =
        fs::read(&path).with_context(|| format!("no resident record at {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {}", path.display()))
}

pub(crate) fn write_resident_record(root: &Path, record: &ResidentRecord) -> Result<()> {
    let path = runtime_dir(root)?.join("resident.json");
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec(record)?)?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Remove the record only while it still names this process, so a resident
/// shutting down never erases its successor's.
pub(crate) fn remove_resident_record(root: &Path) {
    if read_resident_record(root).is_ok_and(|record| record.pid == std::process::id()) {
        let _ = fs::remove_file(resident_record_path(root));
    }
}

/// The socket the running resident recorded.
#[cfg(unix)]
pub(crate) fn socket_path(root: &Path) -> Result<PathBuf> {
    Ok(read_resident_record(root)?.socket)
}

/// The longest Unix socket path the platform accepts, without its trailing NUL.
#[cfg(unix)]
pub(crate) const MAX_SOCKET_PATH: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    107
} else {
    103
};

/// A fresh socket path for this resident process. It goes in the project's
/// runtime directory when it fits the platform limit, and otherwise in the
/// short system temp directory, so a long or non-English project path never
/// costs the socket. The pid keeps names unique among running residents.
#[cfg(unix)]
pub(crate) fn new_socket_path(root: &Path) -> Result<PathBuf> {
    let name = format!("mb-{}.sock", std::process::id());
    let in_project = runtime_dir(root)?.join(&name);
    if in_project.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(in_project);
    }
    Ok(std::env::temp_dir().join(name))
}

pub(crate) fn endpoint(root: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        Ok(socket_path(root)?.display().to_string())
    }
    #[cfg(windows)]
    {
        pipe_name(root)
    }
}

#[cfg(windows)]
pub(crate) fn pipe_name(root: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(std::env::var("USERNAME").unwrap_or_default().as_bytes());
    hash.update([0]);
    hash.update(root.as_os_str().as_encoded_bytes());
    let digest = hash.finalize();
    Ok(format!(
        r"\\.\pipe\membrick-{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

/// The startup marker. Its lock sits beside it as `resident.startup.lock`. The
/// project's own runtime directory already identifies the project, so the
/// names are fixed.
pub(crate) fn starting_path(root: &Path) -> Result<PathBuf> {
    Ok(runtime_dir(root)?.join("resident.starting"))
}

pub(crate) fn starting_lock_path(marker: &Path) -> PathBuf {
    marker.with_extension("startup.lock")
}

pub(crate) fn resident_log_path(root: &Path) -> Result<PathBuf> {
    Ok(runtime_dir(root)?.join("resident.log"))
}

pub(crate) fn wait_for_startup<T>(
    marker: &Path,
    startup_owner: &mut Option<StartupOwner>,
    mut try_request: impl FnMut() -> Option<T>,
    attempts: usize,
    delay: std::time::Duration,
) -> Result<Option<T>> {
    for _ in 0..attempts {
        std::thread::sleep(delay);
        if let Some(response) = try_request() {
            return Ok(Some(response));
        }
        let child_status = startup_owner.as_mut().map(|owner| owner.child.try_wait());
        match child_status {
            Some(Ok(Some(status))) => {
                if let Some(owner) = startup_owner.as_ref() {
                    remove_startup_marker_if_owned(&owner.marker, &owner.token);
                }
                return Err(startup_exit_error(status));
            }
            Some(Err(error)) => {
                // A wait-status error does not prove that the child exited.
                // Keep its startup lock owned until a monitor can establish
                // that it became ready or terminated.
                if let Some(owner) = startup_owner.take() {
                    retain_startup_owner(owner);
                }
                return Err(error).context("could not check resident startup child status");
            }
            Some(Ok(None)) | None => {}
        }
        // The startup owner removes the marker after binding, or removes its
        // own marker if launch fails. If it vanished while the endpoint is
        // still unavailable, fail promptly and let the next request retry.
        if !marker.exists() {
            return Ok(None);
        }
    }
    // A timed-out client must not release its startup lock while the child it
    // launched is still opening the workspace. Otherwise another caller can
    // replace the marker and launch a competing resident.
    if let Some(owner) = startup_owner.take() {
        retain_startup_owner(owner);
    }
    // A timed-out waiter must leave another starter's marker untouched.
    Ok(None)
}

pub(crate) fn retain_startup_owner(owner: StartupOwner) {
    let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(owner)));
    let monitor_owner = owner.clone();
    let monitor = std::thread::Builder::new()
        .name("membrick-resident-startup-monitor".into())
        .spawn(move || {
            let owner = monitor_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(owner) = owner {
                monitor_startup_owner(owner);
            }
        });
    if let Err(error) = monitor {
        tracing::warn!("could not start resident startup monitor; retaining startup ownership until ready: {error}");
        // Thread creation failure is rare, but releasing the lock here would
        // reintroduce the duplicate-start race. Wait fail-closed.
        let owner = owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(owner) = owner {
            monitor_startup_owner(owner);
        }
    }
}

pub(crate) fn monitor_startup_owner(mut owner: StartupOwner) {
    loop {
        if !owner.marker.exists() {
            return;
        }
        match owner.child.try_wait() {
            Ok(Some(_status)) => {
                remove_startup_marker_if_owned(&owner.marker, &owner.token);
                return;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(error) => {
                tracing::warn!("could not monitor resident startup child: {error}");
                // Keep the startup lock while the child remains alive. If it
                // has exited, clean up its marker before releasing ownership.
                loop {
                    match owner.child.try_wait() {
                        Ok(Some(_)) => {
                            remove_startup_marker_if_owned(&owner.marker, &owner.token);
                            return;
                        }
                        Ok(None) | Err(_) => {
                            if !owner.marker.exists() {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct StartupOwner {
    pub(crate) child: Child,
    pub(crate) marker: PathBuf,
    pub(crate) token: String,
    pub(crate) _startup_lock: fs::File,
}

pub(crate) fn start_server(
    root: &Path,
    resident_executable: Option<&Path>,
) -> Result<Option<StartupOwner>> {
    let marker = starting_path(root)?;
    let Some(claim) = claim_startup(&marker)? else {
        return Ok(None);
    };
    let token = claim.token.clone();

    let launch = (|| -> Result<Child> {
        let executable =
            crate::resident_executable::resolve_resident_executable(resident_executable)
                .context("could not locate the Membrick resident CLI")?;
        let log_path = resident_log_path(root)?;
        if std::fs::metadata(&log_path).is_ok_and(|metadata| metadata.len() > 1_048_576) {
            let previous = log_path.with_extension("log.1");
            let _ = std::fs::remove_file(&previous);
            std::fs::rename(&log_path, previous)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        std::process::Command::new(executable)
            .arg("--resident-server")
            .arg("--managed-resident")
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .context("could not start the resident Membrick service")
    })();

    match launch {
        Ok(child) => {
            tracing::info!(resident_pid = child.id(), "started resident");
            Ok(Some(StartupOwner {
                child,
                marker,
                token,
                _startup_lock: claim.lock,
            }))
        }
        Err(error) => {
            remove_startup_marker_if_owned(&marker, &token);
            Err(error)
        }
    }
}

pub(crate) struct StartupClaim {
    pub(crate) token: String,
    pub(crate) lock: fs::File,
}

/// The kernel lock establishes ownership; the `.starting` file is only a
/// status record. A new owner may safely replace stale status after acquiring
/// the lock because locks are released by the OS if the previous starter dies.
pub(crate) fn claim_startup(marker: &Path) -> Result<Option<StartupClaim>> {
    use std::io::Write;
    static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let lock_path = starting_lock_path(marker);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("could not open startup lock {}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        Err(fs::TryLockError::Error(error)) => {
            return Err(error).with_context(|| {
                format!("could not acquire startup lock {}", lock_path.display())
            });
        }
    }

    let token = format!(
        "{}:{}:{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    // We own the kernel lock, so any existing marker can only be stale.
    match fs::remove_file(marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(token.as_bytes()) {
                drop(file);
                let _ = std::fs::remove_file(marker);
                return Err(error.into());
            }
            Ok(Some(StartupClaim { token, lock }))
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn remove_startup_marker_if_owned(marker: &Path, token: &str) {
    if std::fs::read_to_string(marker).is_ok_and(|contents| contents == token) {
        let _ = std::fs::remove_file(marker);
    }
}

pub(crate) fn startup_exit_error(status: ExitStatus) -> anyhow::Error {
    anyhow::anyhow!("resident Membrick service exited before becoming available ({status})")
}
