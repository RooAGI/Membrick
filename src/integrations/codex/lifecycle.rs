//! Translate Codex conversation-only hooks into the shared ownership manager.
//!
//! Codex SessionEnd does not supply an incarnation ID. This compatibility bridge
//! cannot distinguish a delayed end from an earlier resume of the same thread.
//! Do not infer an incarnation from a PID or a timeout.
//!
//! The resident lease is separate: Codex keeps threads open across app and CLI
//! exits, so it follows thread activity through the shared keepalive rather
//! than waiting for a SessionEnd that may never arrive.
use crate::integrations::provider_lifecycle::{self, ResidentKeepalive, SessionToken};
use anyhow::Result;
use std::path::Path;

const PROVIDER: &str = "codex";

pub(super) struct AdapterLifecycle {
    lifecycle: provider_lifecycle::AdapterLifecycle,
    keepalive: ResidentKeepalive,
}
impl AdapterLifecycle {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            lifecycle: provider_lifecycle::AdapterLifecycle::new(root),
            keepalive: ResidentKeepalive::start(root, PROVIDER),
        }
    }
    pub(super) fn bind(&self, session: &str) -> Result<()> {
        self.lifecycle.bind(&token(session)?)?;
        self.keepalive.tool_call(Some(session));
        Ok(())
    }
    /// A tool call that names no thread.
    pub(super) fn tool_call(&self) {
        self.keepalive.tool_call(None);
    }
    pub(super) fn stopped(&self) -> bool {
        let stopped = self.lifecycle.stopped();
        if stopped {
            self.keepalive.end();
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
