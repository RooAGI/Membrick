use super::*;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    thread,
};
use tokio::sync::Mutex;

#[test]
fn memory_options_debug_redacts_api_key() {
    let options = MemoryOptions {
        api_key: Some("secret-test-key".into()),
        ..MemoryOptions::default()
    };
    let debug = format!("{options:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("secret-test-key"));
}

#[test]
fn status_mapping_keeps_timeout_uncertain_and_rate_limit_retryable_by_status() {
    assert!(matches!(
        ensure_success(408, &serde_json::json!({"detail": "timeout"})),
        Err(MemoryError::Uncertain { .. })
    ));
    assert!(matches!(
        ensure_success(429, &serde_json::json!({"detail": "writer busy"})),
        Err(MemoryError::Rejected { status: 429, .. })
    ));
}

#[tokio::test]
async fn async_memory_api_covers_operations_and_keeps_request_and_session_identity() {
    let (client, _, stop, server) = test_service();
    let added = client
        .add_async(
            vec![message("Alice prefers tea")],
            "alice",
            AddOptions {
                request_id: Some("async-durable-request".into()),
                session_id: Some("async-session".into()),
                ..AddOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(added.request_id, "async-durable-request");
    assert_eq!(added.session_id, "async-session");

    let session = client.session("alice", added.session_id.clone());
    let follow_up = session
        .add_async(vec![message("Use a ceramic cup")], AddOptions::default())
        .await
        .unwrap();
    assert_eq!(follow_up.session_id, added.session_id);
    let hits = client
        .search_async("Alice tea", "alice", SearchOptions::default())
        .await
        .unwrap();
    assert!(hits
        .iter()
        .any(|hit| hit.session_id.as_deref() == Some("async-session")));
    assert!(client
        .get_async("missing", "alice", GetOptions::default())
        .await
        .unwrap()
        .is_none());
    assert!(client
        .list_async("alice", ListOptions::default())
        .await
        .unwrap()
        .data
        .is_empty());
    assert!(client
        .update_async("missing", "alice", "new content", UpdateOptions::default())
        .await
        .unwrap()
        .is_none());
    assert!(!client.delete_async("missing", "alice").await.unwrap());
    client.refresh_async().await.unwrap();

    stop.send(()).unwrap();
    server.join().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn async_project_client_sends_add_over_resident_ipc() {
    let root = temp_store().join("async-project-ipc");
    std::fs::create_dir_all(&root).unwrap();

    // This test exercises the real project client transport against a real
    // Unix IPC listener. Managed process startup and idle restart remain
    // covered by tests/memory_client_resident_idle.rs.
    let client = Memory::init_async(&root).await.unwrap();
    let service = Arc::new(Mutex::new(crate::MemoryService::in_memory(
        crate::PipelineOptions::default(),
    )));
    let app = Router::new()
        .route("/v1/memories", post(add_handler))
        .with_state(service.clone());
    let shutdown_root = root.clone();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let resident = tokio::spawn(async move {
        crate::local_ipc::serve(
            app,
            &shutdown_root,
            async move {
                let _ = stopped.await;
                None
            },
            None,
            false,
            None,
        )
        .await
    });

    let record_path = crate::local_ipc::resident_record_path(&root);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if record_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test resident should publish its IPC socket record");

    let receipt = client
        .add_async(
            vec![message("Alice prefers tea")],
            "alice",
            AddOptions {
                request_id: Some("async-project-ipc-request".into()),
                session_id: Some("async-project-ipc-session".into()),
                ..AddOptions::default()
            },
        )
        .await
        .expect("async project add should use the resident IPC path");
    assert_eq!(receipt.request_id, "async-project-ipc-request");
    assert_eq!(receipt.user_id, "alice");
    assert_eq!(receipt.session_id, "async-project-ipc-session");
    let socket = crate::local_ipc::read_resident_record(&root)
        .unwrap()
        .socket;
    assert!(
        socket.exists(),
        "the project request should use the live IPC socket"
    );
    assert_eq!(
        service
            .lock()
            .await
            .diagnostic_records()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1,
        "the IPC resident should have applied the add"
    );

    client.close();
    shutdown.send(()).unwrap();
    resident.await.unwrap().unwrap();
    std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sync_compatibility_api_works_inside_current_thread_runtime() {
    let (client, _, stop, server) = test_service();
    client.refresh().unwrap();
    stop.send(()).unwrap();
    server.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_compatibility_api_works_inside_multi_thread_runtime() {
    let (client, _, stop, server) = test_service();
    client.refresh().unwrap();
    stop.send(()).unwrap();
    server.join().unwrap();
}

type SharedService = Arc<Mutex<crate::MemoryService>>;

/// Mirrors the server's add route: a request with an explicit request ID may
/// omit `session_id`, which is then derived from the user and request ID.
async fn add_handler(
    State(service): State<SharedService>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<Json<AddResponse>, (StatusCode, String)> {
    if body.get("session_id").is_none() {
        let user_id = body["user_id"].as_str().unwrap_or_default();
        let request_id = body["request_id"].as_str().unwrap_or_default();
        body["session_id"] = crate::memory_client::retry_session_id(user_id, request_id).into();
    }
    let request = serde_json::from_value::<AddRequest>(body)
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    service
        .lock()
        .await
        .add(request)
        .map(Json)
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}")))
}

async fn search_handler(
    State(service): State<SharedService>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    service
        .lock()
        .await
        .search(request)
        .map(Json)
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, format!("{error:#}")))
}

async fn refresh_handler() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn list_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": [], "next_cursor": null }))
}

async fn missing_handler() -> StatusCode {
    StatusCode::NOT_FOUND
}

fn test_service() -> (
    Memory,
    SharedService,
    tokio::sync::oneshot::Sender<()>,
    thread::JoinHandle<()>,
) {
    let service = Arc::new(Mutex::new(crate::MemoryService::in_memory(
        crate::PipelineOptions::default(),
    )));
    let app = Router::new()
        .route("/v1/memories", get(list_handler).post(add_handler))
        .route("/v1/memories/search", post(search_handler))
        .route("/v1/memories/refresh", post(refresh_handler))
        .route(
            "/v1/memories/{id}",
            get(missing_handler)
                .patch(missing_handler)
                .delete(missing_handler),
        )
        .with_state(service.clone());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
    });
    let client = Memory::remote(format!("http://{address}"), MemoryOptions::default()).unwrap();
    (client, service, stop, server)
}

fn message(content: &str) -> Message {
    Message {
        role: "user".into(),
        timestamp: None,
        content: content.into(),
        expires_at_ms: None,
        supersedes_id: None,
    }
}

#[test]
fn add_creates_and_restores_sessions_and_scopes_request_ids_by_user() {
    let (client, service, stop, server) = test_service();

    let created = client
        .add(
            vec![message("Alice prefers tea")],
            "alice",
            AddOptions {
                request_id: Some("same-request".into()),
                ..AddOptions::default()
            },
        )
        .unwrap();
    assert_eq!(created.user_id, "alice");
    assert_eq!(created.request_id, "same-request");
    assert!(
        !created.session_id.is_empty(),
        "add should resolve a session ID"
    );

    let restored = client.session("alice", created.session_id.clone());
    assert_eq!(restored.session_id(), created.session_id);
    let follow_up = restored
        .add(
            vec![message("Use a ceramic tea cup")],
            AddOptions::default(),
        )
        .unwrap();
    assert_eq!(follow_up.session_id, created.session_id);
    let restored_search = restored
        .search("ceramic tea", SearchOptions::default())
        .unwrap();
    assert!(restored_search
        .iter()
        .any(|result| result.session_id.as_deref() == Some(created.session_id.as_str())));

    let explicit = client
        .add(
            vec![message("Bob prefers coffee")],
            "bob",
            AddOptions {
                request_id: Some("same-request".into()),
                session_id: Some("bob-conversation".into()),
                ..AddOptions::default()
            },
        )
        .unwrap();
    assert_eq!(explicit.user_id, "bob");
    assert_eq!(explicit.session_id, "bob-conversation");

    let mut service = service.blocking_lock();
    let alice = service
        .search(SearchRequest {
            query: "tea cup".into(),
            options: None,
            user_id: "alice".into(),
            top_k: 10,
            session_id: Some(created.session_id),
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    let bob = service
        .search(SearchRequest {
            query: "coffee".into(),
            options: None,
            user_id: "bob".into(),
            top_k: 10,
            session_id: None,
            scope: None,
            filters: None,
            reference_date: None,
        })
        .unwrap();
    assert!(alice
        .data
        .iter()
        .any(|result| result.content.contains("ceramic")));
    assert!(alice
        .data
        .iter()
        .all(|result| result.user_id.as_deref() == Some("alice")));
    assert!(bob
        .data
        .iter()
        .any(|result| result.content.contains("coffee")));
    assert!(bob
        .data
        .iter()
        .all(|result| result.user_id.as_deref() == Some("bob")));
    assert_eq!(
        service
            .diagnostic_records()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    drop(service);
    let _ = stop.send(());
    server.join().unwrap();
}

#[test]
fn project_client_requires_explicit_project_initialization() {
    let base = temp_store();
    let project = base.join("sdk-only-project");
    std::fs::create_dir_all(&project).unwrap();

    let error = match Memory::new(&project) {
        Ok(_) => panic!("a missing project must not be initialized implicitly"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("not a Membrick project"));
    assert!(!project.join(".membrick").exists());

    let client = Memory::init(&project).unwrap();
    let Connection::Project { root, .. } = &client.connection else {
        panic!("Memory::new must create a project connection");
    };
    assert_eq!(root, &project.canonicalize().unwrap());
    let settings = std::fs::read_to_string(project.join(".membrick/server.toml")).unwrap();
    assert!(settings.contains("project_root ="));

    client.close();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn project_write_without_resident_executable_fails_before_queueing() {
    let base = temp_store();
    let project = base.join("project-without-resident-binary");
    std::fs::create_dir_all(&project).unwrap();
    Memory::init(&project).unwrap();
    let client = Memory::with_options(
        &project,
        MemoryOptions {
            resident_executable: Some(project.join("missing-membrick")),
            ..MemoryOptions::default()
        },
    )
    .unwrap();

    let error = client
        .add(
            vec![message("must not be queued")],
            "alice",
            AddOptions {
                request_id: Some("no-resident-binary-request".into()),
                ..AddOptions::default()
            },
        )
        .unwrap_err();
    assert!(matches!(error, MemoryError::Unavailable { .. }));
    assert!(!project.join(".membrick/runtime/requests").exists());
    client.close();
    std::fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn async_project_write_without_resident_executable_fails_before_queueing() {
    let base = temp_store();
    let project = base.join("async-project-without-resident-binary");
    std::fs::create_dir_all(&project).unwrap();
    Memory::init_async(&project).await.unwrap().close();
    let client = Memory::with_options_async(
        &project,
        MemoryOptions {
            resident_executable: Some(project.join("missing-membrick")),
            ..MemoryOptions::default()
        },
    )
    .await
    .unwrap();

    let error = client
        .add_async(
            vec![message("must not be queued")],
            "alice",
            AddOptions {
                request_id: Some("async-no-resident-binary-request".into()),
                ..AddOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Unavailable { .. }));
    assert!(
        error
            .to_string()
            .contains("could not start project resident"),
        "missing resident executable should be classified before queue fallback: {error}"
    );
    assert!(!project.join(".membrick/runtime/requests").exists());
    client.close();
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn async_project_write_with_exiting_resident_binary_fails_before_queueing() {
    use std::os::unix::fs::PermissionsExt;

    let base = temp_store();
    let project = base.join("async-project-with-exiting-resident-binary");
    std::fs::create_dir_all(&project).unwrap();
    Memory::init_async(&project).await.unwrap().close();
    let executable = project.join("exits-before-resident-startup");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let client = Memory::with_options_async(
        &project,
        MemoryOptions {
            resident_executable: Some(executable),
            ..MemoryOptions::default()
        },
    )
    .await
    .unwrap();

    let error = client
        .add_async(
            vec![message("must not be queued")],
            "alice",
            AddOptions {
                request_id: Some("async-exiting-resident-request".into()),
                ..AddOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Unavailable { .. }));
    assert!(error.to_string().contains("resident exited during startup"));
    assert!(!project.join(".membrick/runtime/requests").exists());
    client.close();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn project_clients_are_pinned_to_distinct_canonical_roots() {
    let base = temp_store();
    let first_path = base.join("project-one");
    let second_path = base.join("project-two");
    std::fs::create_dir_all(&first_path).unwrap();
    std::fs::create_dir_all(&second_path).unwrap();
    crate::state_paths::record_install_settings(&first_path).unwrap();
    crate::state_paths::record_install_settings(&second_path).unwrap();

    let first = Memory::new(&first_path).unwrap();
    let second = Memory::new(&second_path).unwrap();
    let Connection::Project {
        root: first_root, ..
    } = &first.connection
    else {
        panic!("Memory::new must create a project connection");
    };
    let Connection::Project {
        root: second_root, ..
    } = &second.connection
    else {
        panic!("Memory::new must create a project connection");
    };
    assert_ne!(first_root, second_root);
    assert_eq!(first_root, &first_path.canonicalize().unwrap());
    assert_eq!(second_root, &second_path.canonicalize().unwrap());
    first.close();
    second.close();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn cloned_clients_and_sessions_share_the_lifetime_lease_until_last_owner_closes() {
    let base = temp_store();
    let project = base.join("lease-owner-project");
    std::fs::create_dir_all(&project).unwrap();
    let client = Memory::init(&project).unwrap();
    let clone = client.clone();
    let session = client.session("alice", "session-1");
    let leases = project.join(".membrick/runtime/provider-leases");
    let lease_count = || {
        std::fs::read_dir(&leases)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lease"))
            .count()
    };

    assert_eq!(
        lease_count(),
        1,
        "handles for one client share one OS lease"
    );
    client.close();
    assert_eq!(lease_count(), 1, "the clone and session remain owners");
    clone.close();
    assert_eq!(lease_count(), 1, "the session remains an owner");
    drop(session);
    assert_eq!(
        lease_count(),
        0,
        "dropping the final owner releases the lease"
    );

    std::fs::remove_dir_all(base).unwrap();
}

fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    (request_line, body)
}

fn temp_store() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let base = if cfg!(unix) {
        PathBuf::from("/tmp").canonicalize().unwrap()
    } else {
        std::env::temp_dir().canonicalize().unwrap()
    };
    let path = base.join(format!(
        "membrick-client-retry-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn caller_retry_without_session_uses_stable_session_and_deduplicates_after_restart() {
    let root = temp_store();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker_root = root.clone();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let (_, body) = read_http_request(&mut first);
        let mut request_value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(request_value.get("session_id").is_none());
        request_value["session_id"] = serde_json::json!(retry_session_id(
            request_value["user_id"].as_str().unwrap(),
            request_value["request_id"].as_str().unwrap(),
        ));
        let request: AddRequest = serde_json::from_value(request_value).unwrap();
        let first_response =
            crate::MemoryService::at_path(&worker_root, crate::PipelineOptions::default())
                .unwrap()
                .add(request.clone())
                .unwrap();
        drop(first_response);
        drop(first); // Simulate a committed write whose response was lost.

        let (mut retry, _) = listener.accept().unwrap();
        let (_, body) = read_http_request(&mut retry);
        let mut retry_value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(retry_value.get("session_id").is_none());
        retry_value["session_id"] = serde_json::json!(retry_session_id(
            retry_value["user_id"].as_str().unwrap(),
            retry_value["request_id"].as_str().unwrap(),
        ));
        let retry_request: AddRequest = serde_json::from_value(retry_value).unwrap();
        let mut service =
            crate::MemoryService::at_path(&worker_root, crate::PipelineOptions::default()).unwrap(); // Simulate the resident restarting before the retry.
        let response = service.add(retry_request.clone()).unwrap();
        let count = service
            .diagnostic_records()
            .unwrap()
            .as_array()
            .unwrap()
            .len();
        let json = serde_json::to_vec(&response).unwrap();
        write!(
            retry,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            json.len()
        )
        .unwrap();
        retry.write_all(&json).unwrap();
        (request, retry_request, response, count)
    });

    let client = Memory::remote(format!("http://{address}"), MemoryOptions::default()).unwrap();
    let options = AddOptions {
        request_id: Some("durable-retry-key".into()),
        ..AddOptions::default()
    };
    let first = client.add(
        vec![message("A durable decision")],
        "alice",
        options.clone(),
    );
    assert!(
        matches!(first, Err(MemoryError::Uncertain { .. })),
        "first result: {first:?}"
    );
    let retried = client
        .add(vec![message("A durable decision")], "alice", options)
        .unwrap();
    let (original_request, retry_request, response, count) = server.join().unwrap();
    assert_eq!(original_request.request_id, "durable-retry-key");
    assert_eq!(retry_request.request_id, original_request.request_id);
    assert_eq!(retry_request.session_id, original_request.session_id);
    assert_eq!(
        original_request.session_id,
        retry_session_id("alice", "durable-retry-key")
    );
    assert_eq!(retried.request_id, response.request_id);
    assert_eq!(
        count, 1,
        "replaying the same request must create only one document"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retry_session_id_uses_length_prefixed_identity_components() {
    let ordinary = retry_session_id("alice", "durable-retry-key");
    assert_eq!(ordinary, retry_session_id("alice", "durable-retry-key"));
    assert_ne!(
        retry_session_id("a\0b", "c"),
        retry_session_id("a", "b\0c"),
        "component boundaries must not be ambiguous even for rejected identifiers"
    );
}

#[test]
fn remote_classifies_rejections_and_does_not_follow_write_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let redirect_sink = TcpListener::bind("127.0.0.1:0").unwrap();
    redirect_sink.set_nonblocking(true).unwrap();
    let sink_address = redirect_sink.local_addr().unwrap();
    let server = thread::spawn(move || {
        for (status, headers, body) in [
            (
                "422 Unprocessable Entity",
                "",
                br#"{"detail":"bad request"}"#.as_slice(),
            ),
            (
                "307 Temporary Redirect",
                &format!("Location: http://{sink_address}/v1/memories\r\n"),
                b"".as_slice(),
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let client = Memory::remote(format!("http://{address}"), MemoryOptions::default()).unwrap();
    let rejected = client.add(vec![message("bad")], "alice", AddOptions::default());
    assert!(matches!(
        rejected,
        Err(MemoryError::Rejected { status: 422, .. })
    ));
    let redirected = client.add(
        vec![message("maybe written")],
        "alice",
        AddOptions::default(),
    );
    assert!(matches!(redirected, Err(MemoryError::Uncertain { .. })));
    assert!(
        redirect_sink.accept().is_err(),
        "remote client followed a mutating redirect"
    );
    server.join().unwrap();
}

#[test]
fn empty_204_refresh_response_is_accepted() {
    let (client, _, stop, server) = test_service();
    client.refresh().unwrap();
    let _ = stop.send(());
    server.join().unwrap();
}

#[test]
fn remote_get_list_update_delete_decode_results_and_send_ownership_context() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let record = MemoryRecord {
        id: "memory-1".into(),
        content: "original content".into(),
        role: Some("user".into()),
        user_id: "alice".into(),
        session_id: Some("session-1".into()),
        created_at: None,
        expires_at_ms: None,
        supersedes_id: None,
        metadata: Default::default(),
    };
    let server_record = record.clone();
    let server = thread::spawn(move || {
        let response_record = serde_json::to_vec(&server_record).unwrap();
        let response_list = serde_json::to_vec(&ListResponse {
            data: vec![server_record.clone()],
            next_cursor: Some("next-page".into()),
            total_count: 1,
        })
        .unwrap();
        let mut observed = Vec::new();
        for (status, body) in [
            ("200 OK", response_record.clone()),
            ("200 OK", response_list),
            ("200 OK", response_record),
            ("204 No Content", Vec::new()),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let (line, request_body) = read_http_request(&mut stream);
            observed.push((line, request_body));
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
        observed
    });

    let client = Memory::remote(format!("http://{address}"), MemoryOptions::default()).unwrap();
    assert_eq!(
        client
            .get("memory-1", "alice", GetOptions::default())
            .unwrap()
            .unwrap()
            .content,
        "original content"
    );
    let listed = client
        .list(
            "alice",
            ListOptions {
                session_id: Some("session-1".into()),
                ..ListOptions::default()
            },
        )
        .unwrap();
    assert_eq!(listed.data[0].id, "memory-1");
    assert_eq!(listed.next_cursor.as_deref(), Some("next-page"));
    assert_eq!(
        client
            .update(
                "memory-1",
                "alice",
                "updated content",
                UpdateOptions::default(),
            )
            .unwrap()
            .unwrap()
            .id,
        "memory-1"
    );
    assert!(client.delete("memory-1", "alice").unwrap());

    let observed = server.join().unwrap();
    assert!(observed[0].0.starts_with("GET /v1/memories/memory-1?"));
    assert!(observed[0].0.contains("user_id=alice"));
    assert!(observed[1].0.starts_with("GET /v1/memories?"));
    assert!(observed[1].0.contains("user_id=alice"));
    assert!(observed[1].0.contains("session_id=session-1"));
    assert!(observed[2].0.starts_with("PATCH /v1/memories/memory-1 "));
    let update: UpdateRequest = serde_json::from_slice(&observed[2].1).unwrap();
    assert_eq!(update.user_id, "alice");
    assert_eq!(update.memory_id, "memory-1");
    assert_eq!(update.content, "updated content");
    assert!(observed[3].0.starts_with("DELETE /v1/memories/memory-1?"));
    assert!(observed[3].0.contains("user_id=alice"));
}
