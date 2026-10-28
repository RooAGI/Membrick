//! Session ownership independent of a daemon-retained MCP stdin pipe.
//!
//! End events remove only the exact provider and instance token bound to an adapter.
//! An adapter shared by several sessions exits only after its last owner ends.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A provider-issued instance identity, distinct from its resumable conversation ID.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SessionToken {
    provider: String,
    session: String,
    instance: String,
}

impl SessionToken {
    pub(crate) fn new(provider: &str, session: &str, instance: &str) -> Result<Self> {
        for value in [provider, session, instance] {
            anyhow::ensure!(
                !value.trim().is_empty() && value.len() <= 256,
                "invalid provider session identity"
            );
        }
        Ok(Self {
            provider: provider.into(),
            session: session.into(),
            instance: instance.into(),
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Owners {
    sessions: BTreeSet<SessionToken>,
    stopped: bool,
}

pub(crate) struct AdapterLifecycle {
    root: PathBuf,
    path: PathBuf,
}

impl AdapterLifecycle {
    pub(crate) fn new(root: &Path) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            root: root.to_path_buf(),
            path: directory(root).join(format!("{}-{now}-{nonce}.json", std::process::id())),
        }
    }

    pub(crate) fn bind(&self, session: &SessionToken) -> Result<()> {
        let _gate = gate(&self.root)?;
        let mut owners = match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<Owners>(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Owners::default(),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(!owners.stopped, "provider adapter session has ended");
        if owners.sessions.insert(session.to_owned()) {
            fs::write(&self.path, serde_json::to_vec(&owners)?)?;
        }
        Ok(())
    }

    pub(crate) fn stopped(&self) -> bool {
        // Unbound adapters (including clients without hooks) retain normal EOF
        // semantics. Never terminate an active client on a filesystem error.
        if !self.path.exists() {
            return false;
        }
        let read = || -> Result<bool> {
            let _gate = gate(&self.root)?;
            Ok(serde_json::from_slice::<Owners>(&fs::read(&self.path)?)?.stopped)
        };
        read().unwrap_or_else(|error| {
            tracing::warn!("could not check provider adapter ownership: {error:#}");
            false
        })
    }
}

impl Drop for AdapterLifecycle {
    fn drop(&mut self) {
        if !self.path.exists() {
            return;
        }
        if let Ok(_gate) = gate(&self.root) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn end_session(root: &Path, session: &SessionToken) -> Result<()> {
    if !directory(root).exists() {
        return Ok(());
    }
    let _gate = gate(root)?;
    for entry in fs::read_dir(directory(root))? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let read = || -> Result<Owners> { Ok(serde_json::from_slice(&fs::read(&path)?)?) };
        let mut owners = match read() {
            Ok(owners) => owners,
            Err(error) => {
                tracing::warn!(
                    "skipping unreadable provider ownership record {}: {error:#}",
                    path.display()
                );
                continue;
            }
        };
        if owners.sessions.remove(session) {
            owners.stopped = owners.sessions.is_empty();
            fs::write(path, serde_json::to_vec(&owners)?)?;
        }
    }
    let _ = fs::remove_file(activity_path(
        root,
        &session.provider,
        Some(&session.session),
    ));
    Ok(())
}

/// How long an adapter keeps the resident warm after the last tool call or
/// hook it can attribute to its sessions.
pub(crate) const KEEPALIVE_IDLE: Duration = Duration::from_secs(5 * 60);
const KEEPALIVE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Keeps the project resident warm while a provider adapter is in use.
///
/// Hosts can keep an adapter process and its threads open long after the user
/// leaves, and may never send an end event. A lifetime lease would then pin the
/// resident forever, so the lease lasts only while the adapter's sessions show
/// tool calls or hooks within [`KEEPALIVE_IDLE`]. An adapter that has not seen
/// a session ID counts every hook from its provider in the project. Once the
/// lease is released, the resident's own idle timeout applies.
pub(crate) struct ResidentKeepalive {
    shared: Arc<KeepaliveShared>,
}

struct KeepaliveShared {
    root: PathBuf,
    provider: &'static str,
    state: Mutex<KeepaliveState>,
}

struct KeepaliveState {
    lease: Option<crate::local_ipc::ProviderSessionLease>,
    sessions: BTreeSet<String>,
    last_tool_call: SystemTime,
    checked_at: Instant,
    ended: bool,
}

impl ResidentKeepalive {
    /// A keepalive that a background thread checks every few seconds.
    pub(crate) fn start(root: &Path, provider: &'static str) -> Self {
        let keepalive = Self::new(root, provider);
        let shared = Arc::downgrade(&keepalive.shared);
        let spawned = std::thread::Builder::new()
            .name("membrick-resident-keepalive".into())
            .spawn(move || loop {
                std::thread::sleep(KEEPALIVE_CHECK_INTERVAL);
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                shared.refresh();
            });
        if let Err(error) = spawned {
            tracing::warn!(
                "could not start resident keepalive; lease follows tool calls only: {error}"
            );
        }
        keepalive
    }

    pub(crate) fn new(root: &Path, provider: &'static str) -> Self {
        Self {
            shared: Arc::new(KeepaliveShared {
                root: root.to_path_buf(),
                provider,
                state: Mutex::new(KeepaliveState {
                    lease: None,
                    sessions: BTreeSet::new(),
                    last_tool_call: SystemTime::now(),
                    checked_at: Instant::now(),
                    ended: false,
                }),
            }),
        }
    }

    /// Record a tool call, attributed to `session` when the host names one,
    /// and hold the lease.
    pub(crate) fn tool_call(&self, session: Option<&str>) {
        let Ok(mut state) = self.shared.state.lock() else {
            tracing::warn!("resident keepalive lock poisoned");
            return;
        };
        if state.ended {
            return;
        }
        if let Some(session) = session {
            state.sessions.insert(session.to_owned());
        }
        state.last_tool_call = SystemTime::now();
        self.shared.acquire(&mut state);
    }

    /// Release the lease for good: every session of this adapter has ended.
    pub(crate) fn end(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.ended = true;
            state.lease = None;
        }
    }

    /// Release or renew the lease now if a check is due.
    pub(crate) fn refresh(&self) {
        self.shared.refresh();
    }

    #[cfg(test)]
    fn holds_lease(&self) -> bool {
        self.shared.state.lock().unwrap().lease.is_some()
    }

    #[cfg(test)]
    fn backdate(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.last_tool_call = SystemTime::now() - KEEPALIVE_IDLE * 2;
        state.checked_at = Instant::now() - KEEPALIVE_CHECK_INTERVAL * 2;
    }
}

impl KeepaliveShared {
    fn acquire(&self, state: &mut KeepaliveState) {
        if state.lease.is_some() {
            return;
        }
        let lease = format!("{}-session", self.provider);
        match crate::local_ipc::acquire_provider_session_lease(&self.root, &lease) {
            Ok(lease) => state.lease = Some(lease),
            Err(error) => tracing::warn!(
                provider = self.provider,
                "could not hold resident lease: {error:#}"
            ),
        }
    }

    fn refresh(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.ended || state.checked_at.elapsed() < KEEPALIVE_CHECK_INTERVAL {
            return;
        }
        state.checked_at = Instant::now();
        let hooks = if state.sessions.is_empty() {
            last_activity(&self.root, self.provider, None)
        } else {
            state
                .sessions
                .iter()
                .filter_map(|session| last_activity(&self.root, self.provider, Some(session)))
                .max()
        };
        let latest = hooks.map_or(state.last_tool_call, |hook| hook.max(state.last_tool_call));
        let active = latest.elapsed().map_or(true, |idle| idle < KEEPALIVE_IDLE);
        if !active && state.lease.is_some() {
            tracing::info!(
                provider = self.provider,
                idle_seconds = KEEPALIVE_IDLE.as_secs(),
                "provider sessions idle; releasing resident lease"
            );
            state.lease = None;
        } else if active {
            self.acquire(&mut state);
        }
    }
}

/// Record that a provider session is active now. Hooks run in their own
/// processes, so adapters read these records to see hook activity. Each hook
/// marks both its session and its provider. A failure only warns: a hook must
/// never fail because its activity could not be recorded.
pub(crate) fn note_activity(root: &Path, provider: &str, session: Option<&str>) {
    let now = SystemTime::now();
    let session = session.filter(|session| !session.trim().is_empty());
    let record = |path: PathBuf| -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?
            .set_modified(now)?;
        Ok(())
    };
    for path in [
        Some(activity_path(root, provider, None)),
        session.map(|session| activity_path(root, provider, Some(session))),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(error) = record(path) {
            tracing::warn!(provider, "could not record provider activity: {error:#}");
        }
    }
}

fn last_activity(root: &Path, provider: &str, session: Option<&str>) -> Option<SystemTime> {
    fs::metadata(activity_path(root, provider, session))
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// The activity record for one session, or for the whole provider.
fn activity_path(root: &Path, provider: &str, session: Option<&str>) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(provider.as_bytes());
    hash.update([0]);
    if let Some(session) = session {
        hash.update(session.as_bytes());
    }
    let name: String = hash.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let kind = if session.is_some() { "s" } else { "p" };
    directory(root)
        .join("activity")
        .join(format!("{kind}-{name}"))
}

fn directory(root: &Path) -> PathBuf {
    root.join(".membrick/runtime/provider-adapters")
}

fn gate(root: &Path) -> Result<fs::File> {
    fs::create_dir_all(directory(root))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory(root).join("ownership.lock"))?;
    // Bound lock waits so provider hooks can respect their execution budgets,
    // even if another process stalls while holding the gate.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(error) => {
                return Err(anyhow::anyhow!("{error}"))
                    .context("provider ownership lock unavailable")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_instance_end_preserves_resume_other_provider_and_other_owner() {
        let root = std::env::temp_dir().join(format!("mb-instance-{}", std::process::id()));
        let old = SessionToken::new("codex", "conversation", "old-instance").unwrap();
        let new = SessionToken::new("codex", "conversation", "new-instance").unwrap();
        let other = SessionToken::new("claude", "conversation", "old-instance").unwrap();
        let owner = SessionToken::new("codex", "other-conversation", "instance").unwrap();
        let ended = AdapterLifecycle::new(&root);
        ended.bind(&old).unwrap();
        let resumed = AdapterLifecycle::new(&root);
        resumed.bind(&new).unwrap();
        let shared = AdapterLifecycle::new(&root);
        shared.bind(&new).unwrap();
        shared.bind(&owner).unwrap();
        let provider = AdapterLifecycle::new(&root);
        provider.bind(&other).unwrap();
        end_session(&root, &old).unwrap();
        assert!(ended.stopped());
        assert!(!resumed.stopped(), "old end must not stop resumed instance");
        assert!(!provider.stopped(), "provider identities must be isolated");
        assert!(ended.bind(&new).is_err(), "stopped adapter must not revive");
        end_session(&root, &old).unwrap();
        assert!(!resumed.stopped(), "duplicate old end must be harmless");
        end_session(&root, &new).unwrap();
        assert!(resumed.stopped());
        assert!(!shared.stopped(), "other session still owns shared adapter");
        fs::write(directory(&root).join("broken.json"), b"{").unwrap();
        end_session(&root, &owner).unwrap();
        assert!(shared.stopped());
        assert!(!provider.stopped());
        end_session(&root, &other).unwrap();
        assert!(provider.stopped());
        drop((ended, resumed, shared, provider));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idle_keepalive_releases_lease_and_hook_activity_renews_it() {
        let root = std::env::temp_dir().join(format!("mb-keepalive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let bound = ResidentKeepalive::new(&root, "codex");
        bound.tool_call(Some("thread"));
        let unbound = ResidentKeepalive::new(&root, "claude");
        unbound.tool_call(None);
        assert!(bound.holds_lease() && unbound.holds_lease());

        for keepalive in [&bound, &unbound] {
            keepalive.backdate();
            keepalive.refresh();
            assert!(
                !keepalive.holds_lease(),
                "idle adapter must not pin the resident"
            );
        }

        // Another thread of the same provider does not keep a bound adapter warm.
        note_activity(&root, "codex", Some("other-thread"));
        bound.backdate();
        bound.refresh();
        assert!(!bound.holds_lease());
        note_activity(&root, "codex", Some("thread"));
        bound.backdate();
        bound.refresh();
        assert!(bound.holds_lease(), "hook activity must renew the lease");

        // An unbound adapter follows any hook from its own provider only.
        note_activity(&root, "codex", None);
        unbound.backdate();
        unbound.refresh();
        assert!(!unbound.holds_lease());
        note_activity(&root, "claude", Some("session"));
        unbound.backdate();
        unbound.refresh();
        assert!(unbound.holds_lease());

        bound.end();
        bound.tool_call(Some("thread"));
        assert!(
            !bound.holds_lease(),
            "an ended adapter must not take a new lease"
        );
        drop((bound, unbound));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unbound_adapter_uses_transport_lifetime_and_rejects_invalid_identities() {
        let root = std::env::temp_dir().join(format!("mb-unbound-{}", std::process::id()));
        let adapter = AdapterLifecycle::new(&root);
        assert!(!adapter.stopped());
        assert!(SessionToken::new("", "session", "instance").is_err());
        assert!(SessionToken::new("codex", "session", " ").is_err());
        assert!(!root.exists());
    }
}
