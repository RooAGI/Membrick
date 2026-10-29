//! Translate Codex conversation-only hooks into the shared ownership manager.
//!
//! Codex SessionEnd does not supply an incarnation ID. This compatibility bridge
//! cannot distinguish a delayed end from an earlier resume of the same thread.
//! Do not infer an incarnation from a PID or a timeout.
//!
//! The resident lease follows the MCP adapter connection. Codex may retain
//! that connection in its daemon after the UI exits, so SessionEnd also stops
//! the adapter after its last owning conversation ends.
use crate::integrations::provider_lifecycle::{self, ResidentConnectionLease, SessionToken};
use anyhow::Result;
use std::path::Path;

const PROVIDER: &str = "codex";

pub(super) struct AdapterLifecycle {
    lifecycle: provider_lifecycle::AdapterLifecycle,
    resident_lease: ResidentConnectionLease,
}
impl AdapterLifecycle {
    pub(super) fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            lifecycle: provider_lifecycle::AdapterLifecycle::new(root),
            resident_lease: ResidentConnectionLease::start(root, PROVIDER)?,
        })
    }
    pub(super) fn bind(&self, session: &str) -> Result<()> {
        self.lifecycle.bind(&token(session)?)?;
        Ok(())
    }
    pub(super) fn stopped(&self) -> bool {
        let stopped = self.lifecycle.stopped();
        if stopped {
            self.resident_lease.end();
        }
        stopped
    }
}
/// Mark a Codex thread active from a hook process.
pub(super) fn note_activity(root: &Path, session: &str) {
    provider_lifecycle::note_activity(root, PROVIDER, Some(session));
}
pub(super) fn end_session(root: &Path, session: &str) -> Result<()> {
    provider_lifecycle::end_session(root, &token(session)?)
}
fn token(session: &str) -> Result<SessionToken> {
    SessionToken::new(PROVIDER, session, "conversation-only")
}
