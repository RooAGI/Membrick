//! Fail-closed boundary for activating downloaded extensions.
//!
//! Admission verifies an artifact and host policy, but it does not make the
//! artifact executable. This host has no maintained OS sandbox backend and
//! the compiled-in [`membrick_extension_api::ExtensionRegistry`] accepts only
//! statically linked registrations. Keep activation disabled until both
//! boundaries are implemented and reviewed together.

use crate::extension_admission::AdmittedArtifact;
use std::fmt;

/// Missing host capabilities that currently prevent untrusted activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationBlocker {
    /// No maintained, Membrick-owned OS sandbox profile is available.
    MaintainedSandboxProfileUnavailable,
    /// The current registry cannot own a dynamically loaded extension handler.
    OwnedRegistryBridgeUnavailable,
    /// A separate worker has no parent-mediated, per-hop capability RPC bridge.
    CapabilityRpcUnavailable,
    /// Extension-specific document scoping is not wired to admitted grants.
    ScopedDocumentAuthorizationUnavailable,
}

/// A downloaded extension could not be activated safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationUnavailable {
    blockers: &'static [ActivationBlocker],
}

impl ActivationUnavailable {
    /// The host boundaries that must be implemented before activation.
    pub fn blockers(&self) -> &'static [ActivationBlocker] {
        self.blockers
    }
}

impl fmt::Display for ActivationUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("untrusted extension activation is unavailable")
    }
}

impl std::error::Error for ActivationUnavailable {}

/// Placeholder for a successfully activated extension.
///
/// The private field prevents downstream code from constructing this value.
#[derive(Debug)]
pub struct ActivatedExtension {
    _private: (),
}

/// Consume an admitted artifact and fail closed until all required host
/// boundaries exist. This function intentionally cannot return success today.
///
/// Consuming the artifact makes the attempted activation path explicit while
/// ensuring callers cannot pass its bytes to a native loader by accident.
pub fn activate_admitted_extension(
    _artifact: AdmittedArtifact,
) -> Result<ActivatedExtension, ActivationUnavailable> {
    Err(ActivationUnavailable {
        blockers: &[
            ActivationBlocker::MaintainedSandboxProfileUnavailable,
            ActivationBlocker::OwnedRegistryBridgeUnavailable,
            ActivationBlocker::CapabilityRpcUnavailable,
            ActivationBlocker::ScopedDocumentAuthorizationUnavailable,
        ],
    })
}
