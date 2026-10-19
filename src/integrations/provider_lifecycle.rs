//! Session ownership independent of a daemon-retained MCP stdin pipe.
//!
//! End events remove only the exact provider and instance token bound to an adapter.
//! An adapter shared by several sessions exits only after its last owner ends.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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
    Ok(())
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
    fn unbound_adapter_uses_transport_lifetime_and_rejects_invalid_identities() {
        let root = std::env::temp_dir().join(format!("mb-unbound-{}", std::process::id()));
        let adapter = AdapterLifecycle::new(&root);
        assert!(!adapter.stopped());
        assert!(SessionToken::new("", "session", "instance").is_err());
        assert!(SessionToken::new("codex", "session", " ").is_err());
        assert!(!root.exists());
    }
}
