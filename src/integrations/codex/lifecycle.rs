//! Translate Codex conversation-only hooks into the shared ownership manager.
//!
//! Codex SessionEnd does not supply an incarnation ID. This compatibility bridge
//! cannot distinguish a delayed end from an earlier resume of the same thread.
//! Do not infer an incarnation from a PID or a timeout.
use crate::integrations::provider_lifecycle::{self, SessionToken};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(super) struct AdapterLifecycle {
    lifecycle: provider_lifecycle::AdapterLifecycle,
    root: PathBuf,
    resident_lease: Mutex<Option<crate::local_ipc::ProviderSessionLease>>,
}
impl AdapterLifecycle {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            lifecycle: provider_lifecycle::AdapterLifecycle::new(root),
            root: root.to_path_buf(),
            resident_lease: Mutex::new(None),
        }
    }
    pub(super) fn bind(&self, session: &str) -> Result<()> {
        self.lifecycle.bind(&token(session)?)?;
        let mut lease = self
            .resident_lease
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex resident lease lock poisoned"))?;
        if lease.is_none() {
            *lease = Some(crate::local_ipc::acquire_provider_session_lease(
                &self.root,
                "codex-session",
            )?);
        }
        Ok(())
    }
    pub(super) fn stopped(&self) -> bool {
        let stopped = self.lifecycle.stopped();
        if stopped {
            match self.resident_lease.lock() {
                Ok(mut lease) => drop(lease.take()),
                Err(_) => tracing::warn!(
                    "Codex resident lease lock poisoned; lease remains until adapter exit"
                ),
            }
        }
        stopped
    }
}
pub(super) fn end_session(root: &Path, session: &str) -> Result<()> {
    provider_lifecycle::end_session(root, &token(session)?)
}
fn token(session: &str) -> Result<SessionToken> {
    SessionToken::new("codex", session, "conversation-only")
}
