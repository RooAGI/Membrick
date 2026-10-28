//! Local transport for the resident workspace service.
//!
//! Requests use HTTP framing over an OS local stream, so the same Axum routes
//! and JSON contract serve both IPC and the optional TCP listener.

use anyhow::{Context, Result};
use axum::{body::Body, Router};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};
use tower::ServiceExt;

#[derive(Clone, Copy)]
pub(crate) struct LocalIpcRequest;

#[path = "resident_client_transport.rs"]
mod resident_client_transport;
#[path = "resident_file_queue.rs"]
mod resident_file_queue;
#[path = "resident_lifecycle.rs"]
mod resident_lifecycle;

pub(crate) use resident_client_transport::*;
pub(crate) use resident_file_queue::*;
pub(crate) use resident_lifecycle::*;

#[cfg(test)]
async fn request_http(
    bind: &str,
    path: &str,
    body: Vec<u8>,
    root: &Path,
) -> Result<(StatusCode, Value)> {
    anyhow::ensure!(
        http_bind_is_loopback(Some(bind)),
        "resident HTTP fallback only connects to loopback addresses"
    );
    let identity_stream = tokio::net::TcpStream::connect(bind)
        .await
        .context("resident Membrick HTTP listener unavailable")?;
    let (identity_status, identity) =
        request_over_stream(identity_stream, "/health", Vec::new(), hyper::Method::GET)
            .await
            .context("could not verify resident HTTP workspace")?;
    anyhow::ensure!(
        identity_status.is_success(),
        "resident HTTP workspace identity check returned {identity_status}"
    );
    anyhow::ensure!(
        identity.get("workspace_id").and_then(Value::as_str)
            == Some(workspace_identity(root).as_str()),
        "resident HTTP listener belongs to a different workspace"
    );
    let stream = tokio::net::TcpStream::connect(bind)
        .await
        .context("resident Membrick HTTP listener unavailable")?;
    request_over_stream(stream, path, body, hyper::Method::POST).await
}

#[cfg(test)]
async fn request_over_stream<S>(
    stream: S,
    path: &str,
    body: Vec<u8>,
    method: hyper::Method,
) -> Result<(StatusCode, Value)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "membrick.local")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(hyper::body::Bytes::from(body)))?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| serde_json::json!({"detail": error.to_string()}));
    Ok((status, value))
}

#[cfg(test)]
async fn request(root: &Path, path: &str, body: Vec<u8>) -> Result<(StatusCode, Value)> {
    #[cfg(unix)]
    let stream = {
        let endpoint = socket_path(root)?;
        tokio::net::UnixStream::connect(endpoint)
            .await
            .context("resident Membrick IPC server unavailable")?
    };
    #[cfg(windows)]
    let stream = {
        let endpoint = pipe_name(root)?;
        tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint)?
    };

    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::post(path)
        .header(hyper::header::HOST, "membrick.local")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(hyper::body::Bytes::from(body)))?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| serde_json::json!({"detail": error.to_string()}));
    Ok((status, value))
}

/// Serve the workspace endpoint until `shutdown` completes. On Unix the
/// socket file is removed on shutdown, so a stopped owner leaves no stale
/// endpoint behind.
#[cfg(unix)]
fn set_socket_permissions(endpoint: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let result = fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600));
    socket_permissions_result(endpoint, result)
}

#[cfg(unix)]
fn socket_permissions_result(endpoint: &Path, result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
            verify_private_socket_parent(endpoint)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn verify_private_socket_parent(endpoint: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let denied = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot secure resident socket: chmod returned EINVAL and its parent is not owned and private",
        )
    };
    let Some(parent) = endpoint.parent() else {
        return Err(denied());
    };
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(denied());
    }
    Ok(())
}

pub(crate) async fn serve(
    router: Router,
    root: &Path,
    shutdown: impl std::future::Future<Output = Option<fs::File>>,
    http_bind: Option<&str>,
    auto_http_port: bool,
    settings_path: Option<&Path>,
) -> Result<Option<fs::File>> {
    tokio::pin!(shutdown);
    let cleanup_root = root.to_owned();
    match tokio::task::spawn_blocking(move || {
        let recovered = recover_interrupted_claims(&cleanup_root)?;
        let expired = cleanup_expired_queue_files(&cleanup_root, std::time::SystemTime::now())?;
        Ok::<_, anyhow::Error>((recovered, expired))
    })
    .await
    {
        Ok(Ok((recovered, expired))) if recovered > 0 || expired > 0 => tracing::info!(
            "recovered {recovered} interrupted file-queue requests and removed {expired} expired queue files"
        ),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!("file-queue startup cleanup failed: {error:#}"),
        Err(error) => tracing::warn!("file-queue startup cleanup worker failed: {error}"),
    }
    let mut http_listener = if let Some(bind) = http_bind {
        match bind_resident_http(bind, auto_http_port, settings_path).await {
            Ok(listener) => {
                tracing::info!(
                    "Membrick resident HTTP listening on {}",
                    listener.local_addr()?
                );
                Some(listener)
            }
            Err(error) => {
                tracing::warn!(
                    "resident HTTP listener unavailable; continuing with local transports: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let queue_wakeup = std::sync::Arc::new(tokio::sync::Notify::new());
    let _queue_watcher = match queue_dirs(root) {
        Ok((pending, _, _)) => match watch_directory(&pending, queue_wakeup.clone()) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!("resident queue watcher unavailable; using recovery scans: {error}");
                None
            }
        },
        Err(error) => {
            tracing::warn!("resident queue setup failed; using recovery scans: {error:#}");
            None
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use tokio::net::UnixListener;
        // A previous resident's socket is stale once this process owns the
        // workspace, unless that resident still answers.
        if let Ok(previous) = read_resident_record(root) {
            if previous.pid != std::process::id()
                && tokio::net::UnixStream::connect(&previous.socket)
                    .await
                    .is_ok()
            {
                anyhow::bail!("a resident Membrick service already owns this workspace");
            }
            let _ = std::fs::remove_file(&previous.socket);
        }
        // Builds before per-start names used one fixed name per project.
        let _ =
            std::fs::remove_file(runtime_dir(root)?.join(format!("{}.sock", instance_name(root))));
        let endpoint = new_socket_path(root)?;
        let mut listener = if endpoint.exists() {
            // Do not unlink a live owner's socket. Remove only a stale socket
            // path left behind after its process exited; bind below remains
            // the final arbiter if multiple starters race here.
            if tokio::net::UnixStream::connect(&endpoint).await.is_ok() {
                anyhow::bail!("a resident Membrick service already owns this workspace");
            }
            std::fs::remove_file(&endpoint)?;
            match UnixListener::bind(&endpoint) {
                Ok(listener) => Some(listener),
                Err(error) => {
                    tracing::warn!("resident IPC socket unavailable; using file queue: {error}");
                    None
                }
            }
        } else {
            match UnixListener::bind(&endpoint) {
                Ok(listener) => Some(listener),
                Err(error) => {
                    tracing::warn!("resident IPC socket unavailable; using file queue: {error}");
                    None
                }
            }
        };
        if listener.is_some() {
            if let Err(error) = set_socket_permissions(&endpoint) {
                drop(listener.take());
                if let Err(cleanup_error) = std::fs::remove_file(&endpoint) {
                    tracing::warn!(
                        "could not remove unsecured resident socket {}: {cleanup_error}",
                        endpoint.display()
                    );
                }
                return Err(error).with_context(|| {
                    format!("could not secure resident socket {}", endpoint.display())
                });
            }
        }
        // Remember which socket file this process created, so shutdown never
        // removes a newer owner's endpoint at the same path.
        let bound_inode = listener
            .as_ref()
            .and_then(|_| std::fs::metadata(&endpoint).ok())
            .map(|metadata| metadata.ino());
        if listener.is_some() {
            let record = ResidentRecord {
                pid: std::process::id(),
                socket: endpoint.clone(),
            };
            if let Err(error) = write_resident_record(root, &record) {
                tracing::warn!("could not record the resident socket; clients will use HTTP or the file queue: {error:#}");
            }
        }
        let _ = std::fs::remove_file(starting_path(root)?);
        if listener.is_some() {
            tracing::info!("Membrick resident IPC listening on {}", endpoint.display());
        }
        let recovery_period = if _queue_watcher.is_some() {
            std::time::Duration::from_secs(1)
        } else {
            std::time::Duration::from_millis(20)
        };
        let mut recovery = tokio::time::interval(recovery_period);
        let mut queue_cleanup = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(15 * 60),
            std::time::Duration::from_secs(15 * 60),
        );
        let shutdown_gate = loop {
            tokio::select! {
                accepted = async {
                    match listener.as_ref() {
                        Some(listener) => listener.accept().await.map(Some),
                        None => std::future::pending().await,
                    }
                } => match accepted {
                    Ok(Some((stream, _))) => serve_stream(router.clone(), stream, true),
                    Ok(None) => unreachable!(),
                    Err(error) => {
                        tracing::warn!("resident IPC listener failed; continuing with file queue: {error}");
                        listener = None;
                    },
                },
                accepted = async {
                    match http_listener.as_ref() {
                        Some(listener) => listener.accept().await.map(Some),
                        None => std::future::pending().await,
                    }
                } => match accepted {
                    Ok(Some((stream, _))) => serve_stream(router.clone(), stream, http_bind_is_loopback(http_bind)),
                    Ok(None) => unreachable!(),
                    Err(error) => {
                        tracing::warn!("resident HTTP listener stopped: {error}");
                        http_listener = None;
                    },
                },
                _ = queue_wakeup.notified() => if let Err(error) = process_queue_once(&router, root).await {
                    tracing::warn!("resident file-queue poll failed: {error:#}");
                },
                _ = recovery.tick() => if let Err(error) = process_queue_once(&router, root).await {
                    tracing::warn!("resident file-queue recovery scan failed: {error:#}");
                },
                _ = queue_cleanup.tick() => {
                    let cleanup_root = root.to_owned();
                    match tokio::task::spawn_blocking(move || {
                        cleanup_expired_queue_files(&cleanup_root, std::time::SystemTime::now())
                    }).await {
                        Ok(Ok(removed)) if removed > 0 => tracing::info!("removed {removed} expired file-queue artifacts"),
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!("file-queue cleanup failed: {error:#}"),
                        Err(error) => tracing::warn!("file-queue cleanup worker failed: {error}"),
                    }
                }
                gate = &mut shutdown => break gate,
            }
        };
        drop(listener);
        if bound_inode.is_some()
            && std::fs::metadata(&endpoint)
                .is_ok_and(|metadata| Some(metadata.ino()) == bound_inode)
        {
            let _ = std::fs::remove_file(&endpoint);
        }
        remove_resident_record(root);
        tracing::info!("Membrick resident stopped on {}", endpoint.display());
        Ok(shutdown_gate)
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let endpoint = pipe_name(root)?;
        tracing::info!("Membrick resident IPC listening on {endpoint}");
        let recovery_period = if _queue_watcher.is_some() {
            std::time::Duration::from_secs(1)
        } else {
            std::time::Duration::from_millis(20)
        };
        let mut recovery = tokio::time::interval(recovery_period);
        let mut queue_cleanup = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(15 * 60),
            std::time::Duration::from_secs(15 * 60),
        );
        let shutdown_gate = loop {
            let server = ServerOptions::new().create(&endpoint)?;
            let _ = std::fs::remove_file(starting_path(root)?);
            tokio::select! {
                connected = server.connect() => {
                    connected?;
                    serve_stream(router.clone(), server, true);
                }
                accepted = async {
                    match http_listener.as_ref() {
                        Some(listener) => listener.accept().await.map(Some),
                        None => std::future::pending().await,
                    }
                } => match accepted {
                    Ok(Some((stream, _))) => {
                        serve_stream(router.clone(), stream, http_bind_is_loopback(http_bind))
                    }
                    Ok(None) => unreachable!(),
                    Err(error) => {
                        tracing::warn!("resident HTTP listener stopped: {error}");
                        http_listener = None;
                    },
                },
                _ = queue_wakeup.notified() => if let Err(error) = process_queue_once(&router, root).await {
                    tracing::warn!("resident file-queue poll failed: {error:#}");
                },
                _ = recovery.tick() => if let Err(error) = process_queue_once(&router, root).await {
                    tracing::warn!("resident file-queue recovery scan failed: {error:#}");
                },
                _ = queue_cleanup.tick() => {
                    let cleanup_root = root.to_owned();
                    match tokio::task::spawn_blocking(move || {
                        cleanup_expired_queue_files(&cleanup_root, std::time::SystemTime::now())
                    }).await {
                        Ok(Ok(removed)) if removed > 0 => tracing::info!("removed {removed} expired file-queue artifacts"),
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!("file-queue cleanup failed: {error:#}"),
                        Err(error) => tracing::warn!("file-queue cleanup worker failed: {error}"),
                    }
                }
                gate = &mut shutdown => break gate,
            }
        };
        Ok(shutdown_gate)
    }
}

async fn bind_resident_http(
    bind: &str,
    auto_port: bool,
    settings_path: Option<&Path>,
) -> Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => Ok(listener),
        Err(error) if auto_port && error.kind() == std::io::ErrorKind::AddrInUse => {
            let requested: std::net::SocketAddr = bind.parse().with_context(|| {
                format!(
                    "automatic resident HTTP port selection requires an IP:port bind, got {bind}"
                )
            })?;
            const FIRST_PORT: u16 = 8465;
            const LAST_PORT: u16 = 9999;
            let count = u32::from(LAST_PORT - FIRST_PORT) + 1;
            let preferred_offset = requested.port().saturating_sub(FIRST_PORT) as u32;
            for offset in 1..count {
                let port = FIRST_PORT + ((preferred_offset + offset) % count) as u16;
                let candidate = std::net::SocketAddr::new(requested.ip(), port);
                match tokio::net::TcpListener::bind(candidate).await {
                    Ok(listener) => {
                        let actual = listener.local_addr()?;
                        let Some(settings_path) = settings_path else {
                            drop(listener);
                            anyhow::bail!(
                                "automatic resident HTTP port selection requires a settings file"
                            );
                        };
                        if let Err(error) = persist_resident_http_bind(settings_path, actual) {
                            drop(listener);
                            return Err(error).context(
                                "could not persist the resident HTTP port selected after a bind conflict",
                            );
                        }
                        tracing::info!(
                            "resident HTTP port {} was occupied; using {}",
                            requested.port(),
                            actual.port()
                        );
                        return Ok(listener);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error).context(format!(
                "no available resident HTTP port in {FIRST_PORT}..={LAST_PORT}"
            ))
        }
        Err(error) => Err(error.into()),
    }
}

fn persist_resident_http_bind(settings_path: &Path, bind: std::net::SocketAddr) -> Result<()> {
    let contents = fs::read_to_string(&settings_path)
        .with_context(|| format!("could not read {}", settings_path.display()))?;
    let mut settings = contents
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("could not parse {}", settings_path.display()))?;
    let table = settings.as_table_mut();
    table.insert("resident_http_bind", toml_edit::value(bind.to_string()));
    table.insert(
        "dashboard_url",
        toml_edit::value(format!("http://{bind}/dashboard")),
    );

    let temporary = settings_path.with_file_name(format!(
        ".server.toml.{}.{}.tmp",
        std::process::id(),
        bind.port()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    if let Err(error) = file.write_all(settings.to_string().as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(file);
    #[cfg(windows)]
    if settings_path.exists() {
        fs::remove_file(&settings_path)?;
    }
    if let Err(error) = fs::rename(&temporary, &settings_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn serve_stream<S>(router: Router, stream: S, trusted_local: bool)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let service = hyper::service::service_fn(move |request: Request<Incoming>| {
            let router = router.clone();
            async move {
                let mut request = request.map(Body::new);
                if trusted_local {
                    request.extensions_mut().insert(LocalIpcRequest);
                }
                let response = router
                    .oneshot(request)
                    .await
                    .unwrap_or_else(|never| match never {});
                Ok::<Response<Body>, std::convert::Infallible>(response)
            }
        });
        let io = TokioIo::new(stream);
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service)
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        routing::{get, post},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "membrick-local-ipc-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// A folder short enough that a socket fits inside its project
        /// runtime directory. macOS `temp_dir()` is already too long.
        #[cfg(unix)]
        fn short() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = Path::new("/tmp").canonicalize().unwrap().join(format!(
                "mbt-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The resident watches `pending` and scans the queue on every event, and
    /// every scan prepares the queue folders. Rewriting their permissions
    /// changes the folders, which wakes the watcher again, so an idle resident
    /// would scan in a loop.
    #[cfg(unix)]
    #[test]
    fn preparing_existing_queue_dirs_leaves_them_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let root = TempDir::new();
        let (pending, claimed, responses) = queue_dirs(&root.0).unwrap();
        let base = pending.parent().unwrap().to_path_buf();
        let change_times = || {
            [&base, &pending, &claimed, &responses]
                .map(|directory| std::fs::metadata(directory).unwrap().ctime_nsec())
        };
        let before = change_times();
        std::thread::sleep(std::time::Duration::from_millis(20));

        queue_dirs(&root.0).unwrap();

        assert_eq!(
            change_times(),
            before,
            "preparing the queue changed its folders"
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_chmod_einval_is_allowed_only_in_private_owned_parent() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::short();
        let endpoint = root.0.join("resident.sock");
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let einval = || Err(std::io::Error::from_raw_os_error(libc::EINVAL));

        assert!(socket_permissions_result(&endpoint, einval()).is_ok());

        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error = socket_permissions_result(&endpoint, einval()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let symlink_parent = root.0.join("runtime-link");
        std::os::unix::fs::symlink(&root.0, &symlink_parent).unwrap();
        let error =
            socket_permissions_result(&symlink_parent.join("resident.sock"), einval()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn socket_permission_errors_other_than_einval_remain_fatal() {
        let root = TempDir::short();
        let endpoint = root.0.join("resident.sock");
        let error = std::io::Error::from_raw_os_error(libc::EACCES);

        let returned = socket_permissions_result(&endpoint, Err(error)).unwrap_err();

        assert_eq!(returned.raw_os_error(), Some(libc::EACCES));
    }

    #[test]
    fn resident_runtime_paths_are_project_local() {
        #[cfg(unix)]
        let directory = TempDir::short();
        #[cfg(windows)]
        let directory = TempDir::new();
        let root = directory.0.join("project");
        std::fs::create_dir_all(&root).unwrap();

        let runtime = root.join(".membrick").join("runtime");
        assert_eq!(runtime_dir(&root).unwrap(), runtime);
        assert_eq!(
            starting_path(&root).unwrap().parent(),
            Some(runtime.as_path())
        );
        assert_eq!(
            starting_lock_path(&starting_path(&root).unwrap()).parent(),
            Some(runtime.as_path())
        );
        assert_eq!(
            resident_log_path(&root).unwrap(),
            runtime.join("resident.log")
        );
        #[cfg(unix)]
        assert_eq!(
            new_socket_path(&root).unwrap().parent(),
            Some(runtime.as_path())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    /// Replays `ProviderSessionLease::acquire`, which creates each file and
    /// only then locks it, against the resident's periodic lease scan, which
    /// runs without the lifecycle lock.
    #[test]
    fn reveals_bug_lease_scan_deletes_a_lease_before_its_owner_locks_it() {
        let directory = TempDir::new();
        let leases = directory.0.join("provider-leases");
        std::fs::create_dir_all(&leases).unwrap();
        let open_new = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
                .unwrap()
        };

        let marker_path = leases.join("1-1-0.starting");
        let marker = open_new(&marker_path);
        provider_session_is_active(&leases).unwrap();
        marker.try_lock().unwrap();
        let lease = open_new(&leases.join("1-1-0.lease"));
        provider_session_is_active(&leases).unwrap();
        lease.try_lock().unwrap();
        drop(marker);
        let _ = std::fs::remove_file(&marker_path);

        assert!(
            provider_session_is_active(&leases).unwrap(),
            "a provider holding a locked lease must keep the resident alive"
        );
    }

    #[test]
    fn crashed_client_lease_is_recovered_after_its_os_lock_is_released() {
        let directory = TempDir::new();
        let leases = directory.0.join("provider-leases");
        std::fs::create_dir_all(&leases).unwrap();
        let stale = leases.join("dead-client.lease");
        std::fs::write(&stale, b"provider=memory-client").unwrap();

        assert!(!provider_session_is_active(&leases).unwrap());
        remove_released_provider_leases(&leases).unwrap();
        assert!(
            !stale.exists(),
            "an unlocked lease left by a crashed client is stale"
        );
    }

    /// A signal-driven stop must hold the lifecycle lock through flush, as
    /// the lease-driven stop does. Otherwise a client registers during the
    /// flush and starts a replacement that loses the owner lock and exits.
    #[test]
    fn reveals_bug_signal_shutdown_does_not_hold_the_lifecycle_lock() {
        let directory = TempDir::new();
        let leases = directory.0.join(".membrick/runtime/provider-leases");
        std::fs::create_dir_all(&leases).unwrap();
        let provider = std::fs::File::create(leases.join("1-1-0.lease")).unwrap();
        provider.try_lock().unwrap();

        let gate = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(resident_shutdown(
                &directory.0,
                true,
                Duration::from_secs(3),
                async {},
            ));

        assert!(
            gate.is_some(),
            "a signal stop must return the lifecycle lock"
        );
        let other = open_lifecycle_gate(&directory.0).unwrap();
        assert!(
            matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "no client may register while the resident flushes"
        );
    }

    #[test]
    fn failed_startup_wait_does_not_remove_the_owner_marker() {
        let directory = TempDir::new();
        let marker = directory.0.join("resident.starting");
        std::fs::write(&marker, "owner").unwrap();

        let result =
            wait_for_startup::<()>(&marker, &mut None, || None, 1, std::time::Duration::ZERO)
                .unwrap();

        assert!(result.is_none());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "owner");
    }

    #[test]
    fn stale_startup_marker_can_be_recovered_by_a_new_owner() {
        let directory = TempDir::new();
        let marker = directory.0.join("resident.starting");
        std::fs::write(&marker, "dead-process-token").unwrap();

        let claim = claim_startup(&marker).unwrap();

        assert!(claim.is_some(), "a stale marker must not block startup");
        assert_ne!(
            std::fs::read_to_string(marker).unwrap(),
            "dead-process-token",
            "the acquired startup owner should replace stale marker status"
        );
    }

    #[test]
    fn startup_lock_blocks_another_starter_until_the_owner_releases_it() {
        let directory = TempDir::new();
        let marker = directory.0.join("resident.starting");
        let first = claim_startup(&marker).unwrap().unwrap();

        assert!(claim_startup(&marker).unwrap().is_none());
        drop(first);
        assert!(claim_startup(&marker).unwrap().is_some());
    }

    #[test]
    fn concurrent_startup_claims_have_one_owner() {
        let directory = TempDir::new();
        let marker = Arc::new(directory.0.join("resident.starting"));
        let winners = Arc::new(Mutex::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let marker = Arc::clone(&marker);
                let winners = Arc::clone(&winners);
                std::thread::spawn(move || {
                    if let Some(_claim) = claim_startup(marker.as_ref()).unwrap() {
                        *winners.lock().unwrap() += 1;
                        std::thread::sleep(std::time::Duration::from_millis(30));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*winners.lock().unwrap(), 1);
    }

    #[test]
    fn interrupted_queue_claim_returns_uncertain_result_without_replaying_request() {
        let directory = TempDir::new();
        let (_, claimed, responses) = queue_dirs(&directory.0).unwrap();
        let id = "interrupted-write";
        let request = QueueRequest {
            id: id.to_owned(),
            path: "/memory/add".to_owned(),
            payload: serde_json::json!({"text": "may already have been written"}),
        };
        fs::write(
            claimed.join(format!("{id}.json")),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();

        recover_interrupted_claims(&directory.0).unwrap();

        assert!(!claimed.join(format!("{id}.json")).exists());
        let (pending, _, _) = queue_dirs(&directory.0).unwrap();
        assert!(!pending.join(format!("{id}.json")).exists());
        let response: QueueResponse =
            serde_json::from_slice(&fs::read(responses.join(format!("{id}.json"))).unwrap())
                .unwrap();
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE.as_u16());
        assert!(response.body["detail"]
            .as_str()
            .unwrap()
            .contains("outcome is unknown"));
    }

    #[test]
    fn expired_pending_and_orphan_response_files_are_removed_but_recent_files_remain() {
        let directory = TempDir::new();
        let (pending, _, responses) = queue_dirs(&directory.0).unwrap();
        let expired_pending = pending.join("expired.json");
        let recent_pending = pending.join("recent.json");
        let expired_response = responses.join("orphan.json");
        let recent_response = responses.join("active.json");
        for path in [
            &expired_pending,
            &recent_pending,
            &expired_response,
            &recent_response,
        ] {
            fs::write(path, b"stale queue artifact").unwrap();
        }
        let now = std::time::SystemTime::now();
        let old = now - QUEUE_ARTIFACT_RETENTION - std::time::Duration::from_secs(1);
        for path in [&expired_pending, &expired_response] {
            fs::File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }

        cleanup_expired_queue_files(&directory.0, now).unwrap();

        assert!(!expired_pending.exists());
        assert!(!expired_response.exists());
        assert!(recent_pending.exists());
        assert!(recent_response.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_file_queue_requests_are_claimed_once_and_get_their_own_responses() {
        let directory = TempDir::new();
        let handled = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/echo",
                post(
                    |State(handled): State<Arc<AtomicUsize>>,
                     axum::Json(payload): axum::Json<Value>| async move {
                        let sequence = handled.fetch_add(1, Ordering::SeqCst) + 1;
                        axum::Json(
                            serde_json::json!({"echo": payload["value"], "sequence": sequence}),
                        )
                    },
                ),
            )
            .with_state(handled.clone());

        let workers: Vec<_> = (0..2)
            .map(|_| {
                let app = app.clone();
                let root = directory.0.clone();
                let handled = handled.clone();
                tokio::spawn(async move {
                    // Match the client's 60-second wait, so a loaded machine
                    // never leaves requests with no worker to claim them.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                    while handled.load(Ordering::SeqCst) < 24
                        && tokio::time::Instant::now() < deadline
                    {
                        let _ = process_queue_once(&app, &root).await;
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }
                })
            })
            .collect();

        let clients: Vec<_> = (0..24)
            .map(|value| {
                let root = directory.0.clone();
                tokio::task::spawn_blocking(move || {
                    let response =
                        queue_post_json(&root, "/echo", &serde_json::json!({"value": value}))
                            .unwrap();
                    (value, response)
                })
            })
            .collect();
        for client in clients {
            let (expected, (status, response)) = client.await.unwrap();
            assert_eq!(status, StatusCode::OK);
            assert!(response["sequence"].as_u64().unwrap() >= 1);
            assert_eq!(response["echo"].as_u64().unwrap(), expected);
        }
        for worker in workers {
            worker.abort();
        }
        assert_eq!(handled.load(Ordering::SeqCst), 24);
    }

    #[tokio::test]
    async fn one_queue_scan_dispatches_multiple_requests_concurrently() {
        let directory = TempDir::new();
        let rendezvous = Arc::new(tokio::sync::Barrier::new(4));
        let app = Router::new()
            .route(
                "/parallel",
                post(
                    |State(rendezvous): State<Arc<tokio::sync::Barrier>>,
                     axum::Json(payload): axum::Json<Value>| async move {
                        rendezvous.wait().await;
                        axum::Json(payload)
                    },
                ),
            )
            .with_state(rendezvous);
        let (pending, _, _) = queue_dirs(&directory.0).unwrap();
        for index in 0..4 {
            let id = format!("parallel-{index}");
            let request = QueueRequest {
                id: id.clone(),
                path: "/parallel".into(),
                payload: serde_json::json!({"index": index}),
            };
            fs::write(
                pending.join(format!("{id}.json")),
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
        }

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            process_queue_once(&app, &directory.0),
        )
        .await
        .expect("queue requests should be dispatched concurrently")
        .unwrap();
        let (_, _, responses) = queue_dirs(&directory.0).unwrap();
        for index in 0..4 {
            let response: QueueResponse = serde_json::from_slice(
                &fs::read(responses.join(format!("parallel-{index}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(response.status, StatusCode::OK.as_u16());
            assert_eq!(response.body["index"], index);
        }
    }

    #[tokio::test]
    async fn queue_directory_notifications_wake_waiters() {
        let directory = TempDir::new();
        let (pending, _, _) = queue_dirs(&directory.0).unwrap();
        let wake = Arc::new(tokio::sync::Notify::new());
        let _watcher = watch_directory(&pending, wake.clone()).unwrap();
        let write_dir = pending.clone();
        tokio::task::spawn_blocking(move || {
            fs::write(write_dir.join("request.tmp"), b"request").unwrap();
            fs::rename(
                write_dir.join("request.tmp"),
                write_dir.join("request.json"),
            )
            .unwrap();
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), wake.notified())
            .await
            .expect("filesystem change should wake the resident queue");
    }

    #[tokio::test]
    async fn loopback_http_listener_uses_the_same_router_contract() {
        let directory = TempDir::new();
        let identity = super::workspace_identity(&directory.0);
        let handled = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/health",
                get(move || {
                    let identity = identity.clone();
                    async move { axum::Json(serde_json::json!({"workspace_id": identity})) }
                }),
            )
            .route(
                "/echo",
                post(
                    |State(handled): State<Arc<AtomicUsize>>,
                     axum::Json(payload): axum::Json<Value>| async move {
                        handled.fetch_add(1, Ordering::SeqCst);
                        axum::Json(payload)
                    },
                ),
            )
            .with_state(handled.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                serve_stream(app.clone(), stream, true);
            }
        });
        let (status, result) = request_http(
            &bind,
            "/echo",
            serde_json::to_vec(&serde_json::json!({"value": "resident-http"})).unwrap(),
            &directory.0,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["value"], "resident-http");
        assert_eq!(handled.load(Ordering::SeqCst), 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_fallback_refuses_a_resident_from_another_workspace() {
        let directory = TempDir::new();
        let other_workspace_id = super::workspace_identity(&directory.0.join("other"));
        let handled = Arc::new(AtomicUsize::new(0));
        let app =
            Router::new()
                .route(
                    "/health",
                    get(move || {
                        let other_workspace_id = other_workspace_id.clone();
                        async move {
                            axum::Json(serde_json::json!({"workspace_id": other_workspace_id}))
                        }
                    }),
                )
                .route(
                    "/echo",
                    post(
                        |State(handled): State<Arc<AtomicUsize>>,
                         axum::Json(payload): axum::Json<Value>| async move {
                            handled.fetch_add(1, Ordering::SeqCst);
                            axum::Json(payload)
                        },
                    ),
                )
                .with_state(handled.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_stream(app, stream, true);
        });

        let result = request_http(&bind, "/echo", Vec::new(), &directory.0).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("different workspace"));
        assert_eq!(handled.load(Ordering::SeqCst), 0);
        server.await.unwrap();
    }

    /// A project root long enough, in non-English characters, that a socket
    /// inside its runtime directory would exceed the platform limit.
    #[cfg(unix)]
    fn long_non_english_root(directory: &TempDir) -> PathBuf {
        let root = directory
            .0
            .join("项目记忆项目记忆项目记忆")
            .join("メモリープロジェクトメモリー");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(
            root.join(".membrick/runtime/mb-99999.sock")
                .as_os_str()
                .len()
                > MAX_SOCKET_PATH,
            "the fixture must be too long for a socket in the project"
        );
        root
    }

    #[cfg(unix)]
    #[test]
    fn each_resident_gets_a_new_socket_that_fits_the_limit() {
        let directory = TempDir::short();
        let short = directory.0.join("short");
        std::fs::create_dir_all(&short).unwrap();
        let in_project = new_socket_path(&short).unwrap();
        assert_eq!(
            in_project.parent(),
            Some(short.join(".membrick/runtime").as_path())
        );
        assert_eq!(
            in_project.file_name().and_then(|name| name.to_str()),
            Some(format!("mb-{}.sock", std::process::id()).as_str())
        );

        let long = long_non_english_root(&directory);
        let relocated = new_socket_path(&long).unwrap();
        assert!(relocated.as_os_str().len() <= MAX_SOCKET_PATH);
        assert_eq!(relocated.parent(), Some(std::env::temp_dir().as_path()));
    }

    #[test]
    fn a_resident_removes_only_its_own_record() {
        let directory = TempDir::new();
        let own = ResidentRecord {
            pid: std::process::id(),
            socket: directory.0.join("own.sock"),
        };
        write_resident_record(&directory.0, &own).unwrap();
        assert_eq!(read_resident_record(&directory.0).unwrap(), own);
        #[cfg(unix)]
        assert_eq!(socket_path(&directory.0).unwrap(), own.socket);
        remove_resident_record(&directory.0);
        assert!(read_resident_record(&directory.0).is_err());

        let successor = ResidentRecord {
            pid: std::process::id().wrapping_add(1),
            socket: directory.0.join("successor.sock"),
        };
        write_resident_record(&directory.0, &successor).unwrap();
        remove_resident_record(&directory.0);
        assert_eq!(read_resident_record(&directory.0).unwrap(), successor);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_resident_on_a_long_non_english_root_is_reachable_over_ipc() {
        let directory = TempDir::new();
        let root = long_non_english_root(&directory);
        let app = Router::new().route(
            "/echo",
            post(|axum::Json(payload): axum::Json<Value>| async move { axum::Json(payload) }),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve(
                app,
                &server_root,
                async move {
                    let _ = stopped.await;
                    None
                },
                None,
                false,
                None,
            )
            .await
            .unwrap();
        });
        let mut socket = None;
        for _ in 0..200 {
            if let Ok(path) = socket_path(&root) {
                socket = Some(path);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let socket = socket.expect("the resident records its socket once it listens");
        assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH);

        let (status, body) = request(
            &root,
            "/echo",
            serde_json::to_vec(&serde_json::json!({"value": "长路径"})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], "长路径");

        stop.send(()).unwrap();
        server.await.unwrap();
        assert!(!socket.exists(), "shutdown removes the socket");
        assert!(read_resident_record(&root).is_err(), "and the record");
    }
}

#[cfg(test)]
mod resident_http_port_tests {
    use super::bind_resident_http;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "membrick-resident-http-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_projects_claim_distinct_http_ports_and_persist_them() {
        let directory = TempDir::new();
        let first_settings = directory.0.join("first/.membrick/server.toml");
        let second_settings = directory.0.join("second/.membrick/server.toml");
        fs::create_dir_all(first_settings.parent().unwrap()).unwrap();
        fs::create_dir_all(second_settings.parent().unwrap()).unwrap();

        let (port, blocker) = (8465..=9999)
            .find_map(|port| {
                std::net::TcpListener::bind(("127.0.0.1", port))
                    .ok()
                    .map(|listener| (port, listener))
            })
            .expect("test port range should have a free port");
        let config = format!("resident_http_bind = \"127.0.0.1:{port}\"\n");
        fs::write(&first_settings, &config).unwrap();
        fs::write(&second_settings, &config).unwrap();

        let bind = format!("127.0.0.1:{port}");
        let (first, second) = tokio::join!(
            bind_resident_http(&bind, true, Some(&first_settings)),
            bind_resident_http(&bind, true, Some(&second_settings))
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let first_addr = first.local_addr().unwrap();
        let second_addr = second.local_addr().unwrap();
        assert_ne!(first_addr, second_addr);

        for (settings_path, address) in [
            (&first_settings, first_addr),
            (&second_settings, second_addr),
        ] {
            let parsed: toml::Value = fs::read_to_string(settings_path).unwrap().parse().unwrap();
            let expected_bind = address.to_string();
            let expected_dashboard = format!("http://{address}/dashboard");
            assert_eq!(
                parsed["resident_http_bind"].as_str(),
                Some(expected_bind.as_str())
            );
            assert_eq!(
                parsed["dashboard_url"].as_str(),
                Some(expected_dashboard.as_str())
            );
        }
        drop((first, second, blocker));
    }

    #[test]
    fn workspace_identity_is_stable_and_project_specific() {
        let first = Path::new("/tmp/membrick-project-a");
        let second = Path::new("/tmp/membrick-project-b");
        assert_eq!(
            super::workspace_identity(first),
            super::workspace_identity(first)
        );
        assert_ne!(
            super::workspace_identity(first),
            super::workspace_identity(second)
        );
    }
}
