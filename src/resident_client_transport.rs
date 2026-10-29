use super::*;

/// Local client transport error annotated with whether a mutating request may
/// already have reached the resident.
#[derive(Debug)]
pub(crate) enum ClientTransportError {
    BeforeDispatch(String),
    Uncertain(String),
    ResidentUnavailable(String),
}

impl std::fmt::Display for ClientTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeDispatch(message) => formatter.write_str(message),
            Self::Uncertain(message) => formatter.write_str(message),
            Self::ResidentUnavailable(message) => formatter.write_str(message),
        }
    }
}

/// Memory-only local transport. Unlike the legacy helper below, this
/// never switches transports or retries after a POST may have been sent.
pub(crate) fn post_json_with_transport_safe(
    root: &Path,
    path: &str,
    payload: &Value,
    resident_executable: Option<&Path>,
) -> std::result::Result<(StatusCode, Value, &'static str), ClientTransportError> {
    let _request_lease = Some(
        ProviderSessionLease::acquire(root, "request")
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?,
    );
    let body = serde_json::to_vec(payload)
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    let root = root.to_owned();
    let path = path.to_owned();
    let call = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
            .block_on(request_for_memory_client(
                &root,
                &path,
                body.clone(),
                hyper::Method::POST,
                true,
            ))
    };

    match call() {
        Ok((status, body, transport)) => Ok((status, body, transport)),
        Err(ClientTransportError::Uncertain(message)) => {
            Err(ClientTransportError::Uncertain(message))
        }
        Err(ClientTransportError::ResidentUnavailable(message)) => {
            Err(ClientTransportError::ResidentUnavailable(message))
        }
        Err(ClientTransportError::BeforeDispatch(first_error)) => {
            crate::state_paths::validate_project_root(&root).map_err(|error| {
                ClientTransportError::BeforeDispatch(format!(
                    "not starting a resident: {error}; initial transport error: {first_error}"
                ))
            })?;
            start_server(&root, resident_executable).map_err(|error| {
                ClientTransportError::BeforeDispatch(format!(
                    "could not start project resident: {error:#}; initial transport error: {first_error}"
                ))
            })?;
            // Enqueue only after all stream transports failed before dispatch.
            // Once the queue entry is atomically published, any error may be
            // after commit, so do not retry through another transport.
            match queue_post_json_classified(&root, &path, payload) {
                Ok((status, body)) => Ok((status, body, "file-queue")),
                Err(error @ ClientTransportError::BeforeDispatch(_))
                | Err(error @ ClientTransportError::Uncertain(_))
                | Err(error @ ClientTransportError::ResidentUnavailable(_)) => Err(error),
            }
        }
    }
}

pub(crate) fn request_with_transport_safe(
    root: &Path,
    method: hyper::Method,
    path: &str,
    payload: Option<&Value>,
    may_mutate: bool,
    resident_executable: Option<&Path>,
) -> std::result::Result<(StatusCode, Value, &'static str), ClientTransportError> {
    let _request_lease = Some(
        ProviderSessionLease::acquire(root, "request")
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?,
    );
    let body = match payload {
        Some(value) => serde_json::to_vec(value)
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?,
        None => Vec::new(),
    };
    let root = root.to_owned();
    let path = path.to_owned();
    let call = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
            .block_on(request_for_memory_client(
                &root,
                &path,
                body.clone(),
                method.clone(),
                may_mutate,
            ))
    };
    match call() {
        Ok((status, body, transport)) => Ok((status, body, transport)),
        Err(ClientTransportError::Uncertain(message)) => {
            Err(ClientTransportError::Uncertain(message))
        }
        Err(ClientTransportError::ResidentUnavailable(message)) => {
            Err(ClientTransportError::ResidentUnavailable(message))
        }
        Err(ClientTransportError::BeforeDispatch(first_error)) => {
            crate::state_paths::validate_project_root(&root).map_err(|error| {
                ClientTransportError::BeforeDispatch(format!(
                    "not starting a resident: {error}; initial transport error: {first_error}"
                ))
            })?;
            let marker = starting_path(&root).map_err(|error| {
                ClientTransportError::BeforeDispatch(format!(
                    "could not locate resident startup marker: {error:#}"
                ))
            })?;
            let mut startup_owner = start_server(
                &root,
                resident_executable,
            )
            .map_err(|error| {
                ClientTransportError::ResidentUnavailable(format!(
                    "could not start project resident: {error:#}; initial transport error: {first_error}"
                ))
            })?;

            // Startup can take time to open and index the workspace. Retry
            // only failures classified as pre-dispatch; once a mutating call
            // may have reached the resident, return its uncertain result.
            let mut last_error = first_error;
            for _ in 0..1200 {
                std::thread::sleep(Duration::from_millis(50));
                match call() {
                    Ok((status, body, transport)) => return Ok((status, body, transport)),
                    Err(ClientTransportError::Uncertain(message)) => {
                        return Err(ClientTransportError::Uncertain(message));
                    }
                    Err(ClientTransportError::ResidentUnavailable(message)) => {
                        return Err(ClientTransportError::ResidentUnavailable(message));
                    }
                    Err(ClientTransportError::BeforeDispatch(message)) => last_error = message,
                }
                let child_status = startup_owner.as_mut().map(|owner| owner.child.try_wait());
                match child_status {
                    Some(Ok(Some(status))) => {
                        if let Some(owner) = startup_owner.as_ref() {
                            remove_startup_marker_if_owned(&owner.marker, &owner.token);
                        }
                        return Err(ClientTransportError::BeforeDispatch(format!(
                            "resident exited during startup: {}",
                            startup_exit_error(status)
                        )));
                    }
                    Some(Err(error)) => {
                        if let Some(owner) = startup_owner.take() {
                            retain_startup_owner(owner);
                        }
                        return Err(ClientTransportError::BeforeDispatch(format!(
                            "could not check resident startup child status: {error}"
                        )));
                    }
                    Some(Ok(None)) | None => {}
                }
                if !marker.exists() {
                    return Err(ClientTransportError::BeforeDispatch(last_error));
                }
            }
            if let Some(owner) = startup_owner.take() {
                retain_startup_owner(owner);
            }
            Err(ClientTransportError::BeforeDispatch(format!(
                "resident did not become ready before startup timeout: {last_error}"
            )))
        }
    }
}

/// Nonblocking resident request path used by Rust's async SDK methods.
/// Filesystem lease acquisition and process startup run on the blocking pool;
/// socket, HTTP fallback and startup polling stay asynchronous.
pub(crate) async fn request_with_transport_async(
    root: &Path,
    method: hyper::Method,
    path: &str,
    payload: Option<&Value>,
    may_mutate: bool,
    resident_executable: Option<&Path>,
) -> std::result::Result<(StatusCode, Value, &'static str), ClientTransportError> {
    let root = root.to_owned();
    let path = path.to_owned();
    let body = match payload {
        Some(value) => serde_json::to_vec(value)
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?,
        None => Vec::new(),
    };
    let executable = resident_executable.map(Path::to_path_buf);
    let lease_root = root.clone();
    let _request_lease =
        tokio::task::spawn_blocking(move || ProviderSessionLease::acquire(&lease_root, "request"))
            .await
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;

    match request_for_memory_client(&root, &path, body.clone(), method.clone(), may_mutate).await {
        Ok(response) => return Ok(response),
        Err(ClientTransportError::Uncertain(message)) => {
            return Err(ClientTransportError::Uncertain(message));
        }
        Err(ClientTransportError::ResidentUnavailable(message)) => {
            return Err(ClientTransportError::ResidentUnavailable(message));
        }
        Err(ClientTransportError::BeforeDispatch(first_error)) => {
            let validation_root = root.clone();
            tokio::task::spawn_blocking(move || {
                crate::state_paths::validate_project_root(&validation_root)
            })
            .await
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
            .map_err(|error| {
                ClientTransportError::BeforeDispatch(format!(
                    "not starting a resident: {error}; initial transport error: {first_error}"
                ))
            })?;
            let startup_root = root.clone();
            let startup_executable = executable.clone();
            let mut owner = tokio::task::spawn_blocking(move || {
                start_server(&startup_root, startup_executable.as_deref())
            })
            .await
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
            .map_err(|error| {
                ClientTransportError::ResidentUnavailable(format!(
                    "could not start project resident: {error:#}; initial transport error: {first_error}"
                ))
            })?;

            let mut last_error = first_error;
            for _ in 0..1200 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                match request_for_memory_client(
                    &root,
                    &path,
                    body.clone(),
                    method.clone(),
                    may_mutate,
                )
                .await
                {
                    Ok(response) => return Ok(response),
                    Err(ClientTransportError::Uncertain(message)) => {
                        return Err(ClientTransportError::Uncertain(message));
                    }
                    Err(ClientTransportError::ResidentUnavailable(message)) => {
                        return Err(ClientTransportError::ResidentUnavailable(message));
                    }
                    Err(ClientTransportError::BeforeDispatch(message)) => last_error = message,
                }

                let child_status = if let Some(mut startup_owner) = owner.take() {
                    let (startup_owner, status) = tokio::task::spawn_blocking(move || {
                        let status = startup_owner.child.try_wait();
                        if matches!(&status, Ok(Some(_))) {
                            remove_startup_marker_if_owned(
                                &startup_owner.marker,
                                &startup_owner.token,
                            );
                        }
                        (startup_owner, status)
                    })
                    .await
                    .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
                    owner = Some(startup_owner);
                    Some(status)
                } else {
                    None
                };
                match child_status {
                    Some(Ok(Some(status))) => {
                        return Err(ClientTransportError::ResidentUnavailable(format!(
                            "resident exited during startup: {}",
                            startup_exit_error(status)
                        )));
                    }
                    Some(Err(error)) => {
                        if let Some(owner) = owner.take() {
                            retain_startup_owner(owner);
                        }
                        return Err(ClientTransportError::BeforeDispatch(format!(
                            "could not check resident startup child status: {error}"
                        )));
                    }
                    Some(Ok(None)) | None => {}
                }
                if owner.is_none() {
                    let marker_root = root.clone();
                    let marker_exists = tokio::task::spawn_blocking(move || {
                        starting_path(&marker_root).is_ok_and(|marker| marker.exists())
                    })
                    .await
                    .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
                    if !marker_exists {
                        return Err(ClientTransportError::BeforeDispatch(last_error));
                    }
                }
            }
            if let Some(owner) = owner.take() {
                retain_startup_owner(owner);
            }
            Err(ClientTransportError::BeforeDispatch(format!(
                "resident did not become ready before startup timeout: {last_error}"
            )))
        }
    }
}

pub(crate) async fn post_json_with_transport_async(
    root: &Path,
    path: &str,
    payload: &Value,
    resident_executable: Option<&Path>,
) -> std::result::Result<(StatusCode, Value, &'static str), ClientTransportError> {
    match request_with_transport_async(
        root,
        hyper::Method::POST,
        path,
        Some(payload),
        true,
        resident_executable,
    )
    .await
    {
        Err(ClientTransportError::BeforeDispatch(first_error)) => {
            let root = root.to_owned();
            let path = path.to_owned();
            let payload = payload.clone();
            tokio::task::spawn_blocking(move || queue_post_json_classified(&root, &path, &payload))
                .await
                .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
                .map(|(status, body)| (status, body, "file-queue"))
                .map_err(|error| match error {
                    ClientTransportError::BeforeDispatch(message) => {
                        ClientTransportError::BeforeDispatch(format!(
                            "{first_error}; queue fallback: {message}"
                        ))
                    }
                    ClientTransportError::Uncertain(message) => {
                        ClientTransportError::Uncertain(message)
                    }
                    ClientTransportError::ResidentUnavailable(message) => {
                        ClientTransportError::ResidentUnavailable(message)
                    }
                })
        }
        result => result,
    }
}

pub(crate) async fn request_for_memory_client(
    root: &Path,
    path: &str,
    body: Vec<u8>,
    method: hyper::Method,
    may_mutate: bool,
) -> std::result::Result<(StatusCode, Value, &'static str), ClientTransportError> {
    match request_socket_for_memory_client(root, path, body.clone(), method.clone(), may_mutate)
        .await
    {
        Ok((status, body)) => Ok((status, body, "ipc")),
        Err(ClientTransportError::Uncertain(message)) => {
            Err(ClientTransportError::Uncertain(message))
        }
        Err(ClientTransportError::ResidentUnavailable(message)) => {
            Err(ClientTransportError::ResidentUnavailable(message))
        }
        Err(ClientTransportError::BeforeDispatch(ipc_error)) => {
            let settings_root = root.to_owned();
            let bind = tokio::task::spawn_blocking(move || configured_http_bind(&settings_root))
                .await
                .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
            let Some(bind) = bind else {
                return Err(ClientTransportError::BeforeDispatch(ipc_error));
            };
            request_http_for_memory_client(&bind, path, body, root, method, may_mutate)
                .await
                .map(|(status, body)| (status, body, "http"))
                .map_err(|error| match error {
                    ClientTransportError::BeforeDispatch(http_error) => {
                        ClientTransportError::BeforeDispatch(format!(
                            "IPC failed before dispatch ({ipc_error}); HTTP failed before dispatch ({http_error})"
                        ))
                    }
                    ClientTransportError::Uncertain(message) => {
                        ClientTransportError::Uncertain(format!(
                            "HTTP outcome uncertain after IPC failed before dispatch ({ipc_error}): {message}"
                        ))
                    }
                    ClientTransportError::ResidentUnavailable(message) => {
                        ClientTransportError::ResidentUnavailable(message)
                    }
                })
        }
    }
}

pub(crate) async fn request_socket_for_memory_client(
    root: &Path,
    path: &str,
    body: Vec<u8>,
    method: hyper::Method,
    may_mutate: bool,
) -> std::result::Result<(StatusCode, Value), ClientTransportError> {
    #[cfg(unix)]
    let socket_root = root.to_owned();
    #[cfg(unix)]
    let socket = tokio::task::spawn_blocking(move || socket_path(&socket_root))
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    #[cfg(windows)]
    let stream = {
        let name = pipe_name(root)
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
        tokio::task::spawn_blocking(move || {
            tokio::net::windows::named_pipe::ClientOptions::new().open(name)
        })
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?
    };

    request_over_stream_for_memory_client(stream, path, body, method, may_mutate).await
}

pub(crate) async fn request_http_for_memory_client(
    bind: &str,
    path: &str,
    body: Vec<u8>,
    root: &Path,
    method: hyper::Method,
    may_mutate: bool,
) -> std::result::Result<(StatusCode, Value), ClientTransportError> {
    let bind = bind.to_owned();
    let addresses = tokio::task::spawn_blocking(move || resolved_loopback_http_addresses(&bind))
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    let Some(addresses) = addresses else {
        return Err(ClientTransportError::BeforeDispatch(
            "resident HTTP fallback only connects to loopback addresses".into(),
        ));
    };
    let mut last_error = None;
    let mut connected = None;
    for address in addresses {
        match tokio::net::TcpStream::connect(address).await {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = connected.ok_or_else(|| {
        ClientTransportError::BeforeDispatch(last_error.map_or_else(
            || "resident HTTP address did not resolve".into(),
            |e| e.to_string(),
        ))
    })?;
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let (identity_status, identity) = request_with_sender_for_memory_client(
        &mut sender,
        "/health",
        Vec::new(),
        hyper::Method::GET,
        false,
    )
    .await?;
    if !identity_status.is_success() {
        return Err(ClientTransportError::BeforeDispatch(format!(
            "resident HTTP workspace identity check returned {identity_status}"
        )));
    }
    let identity_root = root.to_owned();
    let expected_workspace_id =
        tokio::task::spawn_blocking(move || workspace_identity(&identity_root))
            .await
            .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    if identity.get("workspace_id").and_then(Value::as_str) != Some(expected_workspace_id.as_str())
    {
        return Err(ClientTransportError::BeforeDispatch(
            "resident HTTP listener belongs to a different workspace".into(),
        ));
    }
    request_with_sender_for_memory_client(&mut sender, path, body, method, may_mutate).await
}

pub(crate) async fn request_over_stream_for_memory_client<S>(
    stream: S,
    path: &str,
    body: Vec<u8>,
    method: hyper::Method,
    may_mutate: bool,
) -> std::result::Result<(StatusCode, Value), ClientTransportError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    request_with_sender_for_memory_client(&mut sender, path, body, method, may_mutate).await
}

pub(crate) async fn request_with_sender_for_memory_client(
    sender: &mut hyper::client::conn::http1::SendRequest<Full<hyper::body::Bytes>>,
    path: &str,
    body: Vec<u8>,
    method: hyper::Method,
    may_mutate: bool,
) -> std::result::Result<(StatusCode, Value), ClientTransportError> {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "membrick.local")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(hyper::body::Bytes::from(body)))
        .map_err(|error| ClientTransportError::BeforeDispatch(error.to_string()))?;
    let response = sender.send_request(request).await.map_err(|error| {
        if may_mutate {
            ClientTransportError::Uncertain(error.to_string())
        } else {
            ClientTransportError::BeforeDispatch(error.to_string())
        }
    })?;
    let status = response.status();
    let bytes = match response.into_body().collect().await {
        Ok(bytes) => bytes.to_bytes(),
        Err(error) if status.is_client_error() => {
            return Ok((
                status,
                Value::String(format!("error response body could not be read: {error}")),
            ));
        }
        Err(error) if may_mutate => {
            return Err(ClientTransportError::Uncertain(error.to_string()));
        }
        Err(error) => {
            return Err(ClientTransportError::BeforeDispatch(error.to_string()));
        }
    };
    if bytes.is_empty() {
        return Ok((status, Value::Null));
    }
    let value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if status.is_client_error() => {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(error) if may_mutate => {
            return Err(ClientTransportError::Uncertain(format!(
                "invalid JSON response: {error}"
            )));
        }
        Err(error) => {
            return Err(ClientTransportError::BeforeDispatch(format!(
                "invalid health response: {error}"
            )));
        }
    };
    Ok((status, value))
}

pub(crate) fn configured_http_bind(root: &Path) -> Option<String> {
    let settings = fs::read_to_string(root.join(".membrick").join("server.toml")).ok()?;
    settings
        .parse::<toml::Value>()
        .ok()?
        .get("resident_http_bind")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn transport_endpoint(root: &Path, transport: &str) -> Option<String> {
    match transport {
        "ipc" => endpoint(root).ok(),
        "http" => configured_http_bind(root),
        "file-queue" => Some(
            root.join(".membrick/runtime/requests")
                .display()
                .to_string(),
        ),
        _ => None,
    }
}

pub(crate) fn http_bind_is_loopback(bind: Option<&str>) -> bool {
    bind.and_then(resolved_loopback_http_addresses).is_some()
}

fn resolved_loopback_http_addresses(bind: &str) -> Option<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let addresses = bind.to_socket_addrs().ok()?.collect::<Vec<_>>();
    (!addresses.is_empty() && addresses.iter().all(|address| address.ip().is_loopback()))
        .then_some(addresses)
}

pub(crate) fn workspace_identity(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
