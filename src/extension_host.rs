//! Generic runtime for compiled-in extensions.
//!
//! The host discovers extensions from the link-time registry and supplies
//! scoped invocation contexts. Domain behavior remains in the extension
//! crates.
//!
//! Membrick names no extension crate. A distribution binary that links
//! extensions must reference each extension crate from its own code before
//! calling `membrick::run_binary`. Registrations live in a `linkme` slice that
//! nothing else names, so the linker drops a crate the binary never
//! references and its tools silently disappear.

use membrick_extension_api::{ExtensionError, ExtensionRegistry};
use std::sync::OnceLock;

static REGISTRY: OnceLock<ExtensionRegistry> = OnceLock::new();
static INITIALIZED: OnceLock<Result<(), ExtensionError>> = OnceLock::new();
static SHUT_DOWN: OnceLock<Result<(), ExtensionError>> = OnceLock::new();

pub(crate) fn registry() -> &'static ExtensionRegistry {
    REGISTRY.get_or_init(|| {
        ExtensionRegistry::from_linked().expect("linked extension registrations must be valid")
    })
}

/// Initialize extensions once in declared dependency order.
pub(crate) fn initialize() -> Result<(), ExtensionError> {
    INITIALIZED.get_or_init(|| registry().initialize()).clone()
}

/// Shut down initialized extensions once in reverse dependency order.
pub(crate) fn shutdown() -> Result<(), ExtensionError> {
    initialize()?;
    SHUT_DOWN.get_or_init(|| registry().shutdown()).clone()
}
