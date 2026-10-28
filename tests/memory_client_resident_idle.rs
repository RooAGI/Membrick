use membrick::{AddOptions, Memory, MemoryOptions, Message};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn project_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("membrick-sdk-idle-{}-{nonce}", std::process::id()))
}

fn set_short_idle_timeout(root: &Path) {
    let settings = root.join(".membrick/server.toml");
    let current = std::fs::read_to_string(&settings).expect("project settings should exist");
    let rest = current
        .lines()
        .filter(|line| {
            !line
                .trim_start()
                .starts_with("resident_idle_timeout_seconds")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        settings,
        format!("resident_idle_timeout_seconds = 5\n{rest}\n"),
    )
    .expect("the test project should use the minimum idle timeout");
}

fn wait_for_shutdowns(root: &Path, expected: usize) -> String {
    let log_path = root.join(".membrick/runtime/resident.log");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log
            .matches("shutdown complete; pending writes flushed")
            .count()
            >= expected
        {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "managed resident did not shut down {expected} times; log:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
fn client_and_session_leases_keep_resident_warm_then_release_and_restart() {
    let root = project_root();
    let client = Memory::init_with_options(
        &root,
        MemoryOptions {
            resident_executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_membrick"))),
            ..MemoryOptions::default()
        },
    )
    .expect("SDK should initialize the test project");
    set_short_idle_timeout(&root);

    let first = client
        .add(
            vec![message("before the managed resident idles out")],
            "sdk-idle-user",
            AddOptions {
                request_id: Some("sdk-idle-before".into()),
                ..AddOptions::default()
            },
        )
        .expect("first SDK write should start the resident");
    assert_eq!(first.request_id, "sdk-idle-before");

    std::thread::sleep(Duration::from_secs(6));
    let warm_log =
        std::fs::read_to_string(root.join(".membrick/runtime/resident.log")).unwrap_or_default();
    assert_eq!(
        warm_log
            .matches("shutdown complete; pending writes flushed")
            .count(),
        0
    );

    let clone = client.clone();
    let session = client.session("sdk-idle-user", first.session_id.clone());
    client.close();
    clone.close();
    std::thread::sleep(Duration::from_secs(6));
    let session_kept_resident =
        std::fs::read_to_string(root.join(".membrick/runtime/resident.log")).unwrap_or_default();
    assert_eq!(
        session_kept_resident
            .matches("shutdown complete; pending writes flushed")
            .count(),
        0,
        "closing other handles must not release the live session's lease"
    );

    session.close();
    let first_shutdown_log = wait_for_shutdowns(&root, 1);
    assert!(first_shutdown_log.contains("resident idle timeout elapsed; shutting down"));

    let reopened = Memory::with_options(
        &root,
        MemoryOptions {
            resident_executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_membrick"))),
            ..MemoryOptions::default()
        },
    )
    .expect("a fresh client should reacquire a lease after shutdown");
    let second = reopened
        .add(
            vec![message("after a fresh client restarted the resident")],
            "sdk-idle-user",
            AddOptions {
                request_id: Some("sdk-idle-after".into()),
                ..AddOptions::default()
            },
        )
        .expect("the same SDK client should start a new resident and write successfully");
    assert_eq!(second.request_id, "sdk-idle-after");
    assert_eq!(second.user_id, "sdk-idle-user");
    assert_ne!(first.session_id, second.session_id);

    reopened.close();
    let final_log = wait_for_shutdowns(&root, 2);
    assert!(
        final_log.matches("Membrick resident IPC listening").count() >= 2,
        "the SDK should have caused two resident startups; log:\n{final_log}"
    );

    std::fs::remove_dir_all(&root).expect("test project should be cleaned up");
}
