use super::*;

#[derive(Serialize, Deserialize)]
pub(crate) struct QueueRequest {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) payload: Value,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct QueueResponse {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

pub(crate) const QUEUE_ARTIFACT_RETENTION: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

pub(crate) fn queue_dirs(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let base = root.join(".membrick").join("runtime").join("requests");
    let pending = base.join("pending");
    let claimed = base.join("claimed");
    let responses = base.join("responses");
    for directory in [&pending, &claimed, &responses] {
        fs::create_dir_all(directory)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only fix permissions that differ. Setting them changes the folder,
        // and the resident's watcher on `pending` would wake and scan again.
        for directory in [&base, &pending, &claimed, &responses] {
            if fs::metadata(directory)?.permissions().mode() & 0o777 != 0o700 {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    Ok((pending, claimed, responses))
}

/// Recover claims left by a resident that exited unexpectedly. The request
/// might already have mutated MemoryService before the crash, so never replay
/// it. Return an explicit uncertain result to a still-waiting client instead.
pub(crate) fn recover_interrupted_claims(root: &Path) -> Result<usize> {
    let (_, claimed, responses) = queue_dirs(root)?;
    let mut recovered = 0;
    for entry in fs::read_dir(&claimed)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let request = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<QueueRequest>(&bytes).ok());
        if let Some(request) = request {
            if path.file_stem().and_then(|value| value.to_str()) != Some(request.id.as_str()) {
                tracing::warn!(
                    "removing abandoned queue claim with mismatched id {}",
                    path.display()
                );
                fs::remove_file(path)?;
                recovered += 1;
                continue;
            }
            let response_path = responses.join(format!("{}.json", request.id));
            if !response_path.exists() {
                let response = QueueResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                    body: serde_json::json!({
                        "detail": "resident stopped while processing this request; outcome is unknown and the request was not replayed"
                    }),
                };
                let temporary = responses.join(format!("{}.recovery.tmp", request.id));
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec(&response)?)?;
                fs::rename(temporary, response_path)?;
            }
        } else {
            tracing::warn!(
                "removing malformed abandoned queue claim {}",
                path.display()
            );
        }
        fs::remove_file(path)?;
        recovered += 1;
    }
    Ok(recovered)
}

/// Remove old pending work, orphaned responses, and incomplete temporary
/// files. Claimed requests are handled separately at resident startup, when
/// the workspace owner lock proves the previous resident is gone.
pub(crate) fn cleanup_expired_queue_files(
    root: &Path,
    now: std::time::SystemTime,
) -> Result<usize> {
    let (pending, claimed, responses) = queue_dirs(root)?;
    let mut removed = 0;
    for directory in [&pending, &claimed, &responses] {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let extension = path.extension().and_then(|value| value.to_str());
            if extension != Some("json") && extension != Some("tmp") {
                continue;
            }
            if directory == &claimed && extension == Some("json") {
                continue;
            }
            let modified = match entry.metadata()?.modified() {
                Ok(modified) => modified,
                Err(_) => continue,
            };
            if now.duration_since(modified).unwrap_or_default() >= QUEUE_ARTIFACT_RETENTION {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

pub(crate) fn watch_directory(
    directory: &Path,
    wake: std::sync::Arc<tokio::sync::Notify>,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            wake.notify_one();
        }
    })?;
    watcher.watch(directory, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

pub(crate) fn queue_post_json(
    root: &Path,
    path: &str,
    payload: &Value,
) -> Result<(StatusCode, Value)> {
    let mut published = false;
    queue_post_json_impl(root, path, payload, &mut published)
}

pub(crate) fn queue_post_json_classified(
    root: &Path,
    path: &str,
    payload: &Value,
) -> std::result::Result<(StatusCode, Value), ClientTransportError> {
    let mut published = false;
    let result = queue_post_json_impl(root, path, payload, &mut published);
    result.map_err(|error| {
        if published {
            ClientTransportError::Uncertain(error.to_string())
        } else {
            ClientTransportError::BeforeDispatch(error.to_string())
        }
    })
}

pub(crate) fn queue_post_json_impl(
    root: &Path,
    path: &str,
    payload: &Value,
    published: &mut bool,
) -> Result<(StatusCode, Value)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let (pending, _, responses) = queue_dirs(root)?;
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let mut response_watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = events_tx.send(());
            }
        })
        .ok();
    if response_watcher.as_mut().is_some_and(|watcher| {
        watcher
            .watch(&responses, RecursiveMode::NonRecursive)
            .is_err()
    }) {
        response_watcher = None;
    }
    let id = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let request = QueueRequest {
        id: id.clone(),
        path: path.to_owned(),
        payload: payload.clone(),
    };
    let temporary = pending.join(format!("{id}.tmp"));
    let target = pending.join(format!("{id}.json"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&request)?)?;
    fs::rename(temporary, target)?;
    *published = true;

    let response = responses.join(format!("{id}.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        match fs::read(&response) {
            Ok(bytes) => {
                let value: QueueResponse = serde_json::from_slice(&bytes)?;
                let _ = fs::remove_file(response);
                let status =
                    StatusCode::from_u16(value.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return Ok((status, value.body));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if response_watcher.is_some() {
                    let _ = events_rx.recv_timeout(std::time::Duration::from_millis(250));
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("resident file-queue request timed out; outcome may be unknown")
}

pub(crate) async fn process_queue_once(router: &Router, root: &Path) -> Result<()> {
    const MAX_IN_FLIGHT: usize = 32;
    let root = root.to_owned();
    let claims =
        tokio::task::spawn_blocking(move || claim_pending_requests(&root, MAX_IN_FLIGHT)).await??;
    let mut tasks = tokio::task::JoinSet::new();
    for (claim, responses) in claims {
        let router = router.clone();
        tasks.spawn(async move {
            if let Err(error) = process_claimed_request(router, claim.clone(), responses).await {
                tracing::warn!("resident file-queue request failed: {error:#}");
            }
            let claim_to_remove = claim;
            let _ = tokio::task::spawn_blocking(move || fs::remove_file(claim_to_remove)).await;
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Atomically claim a bounded batch. Filesystem enumeration, rename, and file
/// reads stay off the resident's async executor threads.
pub(crate) fn claim_pending_requests(root: &Path, limit: usize) -> Result<Vec<(PathBuf, PathBuf)>> {
    let (pending, claimed, responses) = queue_dirs(root)?;
    let mut entries = fs::read_dir(&pending)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut claims = Vec::with_capacity(limit.min(entries.len()));
    for entry in entries.into_iter().take(limit) {
        let source = entry.path();
        let claim = claimed.join(entry.file_name());
        // Atomic rename gives exactly one resident worker ownership of a
        // request, even if multiple workers are accidentally started.
        if fs::rename(&source, &claim).is_ok() {
            claims.push((claim, responses.clone()));
        }
    }
    Ok(claims)
}

pub(crate) async fn process_claimed_request(
    router: Router,
    claim: PathBuf,
    responses: PathBuf,
) -> Result<()> {
    let claim_for_read = claim.clone();
    let request: QueueRequest = tokio::task::spawn_blocking(move || {
        serde_json::from_slice(&fs::read(claim_for_read)?).map_err(anyhow::Error::from)
    })
    .await??;
    anyhow::ensure!(
        request.path.starts_with('/') && !request.path.starts_with("//"),
        "invalid queued route"
    );
    let mut http_request = Request::post(&request.path)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&request.payload)?))?;
    http_request.extensions_mut().insert(LocalIpcRequest);
    let response = router
        .oneshot(http_request)
        .await
        .unwrap_or_else(|never| match never {});
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| serde_json::json!({"detail": error.to_string()}));
    let output = QueueResponse {
        status: status.as_u16(),
        body,
    };
    let response_path = responses.join(format!("{}.json", request.id));
    let temporary = responses.join(format!("{}.tmp", request.id));
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&output)?)?;
        fs::rename(temporary, response_path)?;
        Ok(())
    })
    .await??;
    Ok(())
}
