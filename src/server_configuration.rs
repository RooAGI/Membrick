//! Shared reader for project and standalone HTTP server settings.

use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_IGNORE_PATHS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "coverage",
    ".git",
    "venv",
    ".tox",
    "site-packages",
];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ServerConfiguration {
    pub(crate) bind: Option<String>,
    pub(crate) resident_http_bind: Option<String>,
    pub(crate) resident_http_auto_port: Option<bool>,
    /// Generated convenience URL; the listener is configured separately.
    pub(crate) dashboard_url: Option<String>,
    pub(crate) index: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) server_token: Option<String>,
    pub(crate) server_token_file: Option<PathBuf>,
    pub(crate) tenant_id: Option<String>,
    pub(crate) jwt_secret: Option<String>,
    pub(crate) ignore_paths: Option<Vec<String>>,
    pub(crate) allow_non_loopback: Option<bool>,
    pub(crate) adaptive_segment_max_n: Option<usize>,
    pub(crate) freshness: Option<crate::temporal::FreshnessPolicy>,
    pub(crate) logging: Option<LoggingConfiguration>,
    pub(crate) service_registration: Option<ServiceRegistrationConfiguration>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ServiceRegistrationConfiguration {
    /// HTTP base URL of the membrick-service dispatcher.
    pub(crate) dispatcher_url: Option<String>,
    /// Heartbeat interval. The dispatcher expires local registrations after 20s.
    pub(crate) heartbeat_interval_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct LoggingConfiguration {
    pub(crate) level: Option<String>,
}

impl ServerConfiguration {
    /// Resolve the server settings path used by the standalone server.
    pub(crate) fn settings_path(
        explicit_path: Option<&Path>,
        project_root: Option<&Path>,
    ) -> anyhow::Result<PathBuf> {
        if let Some(path) = explicit_path {
            return Ok(path.to_path_buf());
        }
        let root = match project_root {
            Some(root) => root.to_path_buf(),
            None => std::env::current_dir()?,
        };
        Ok(root.join(".membrick").join("server.toml"))
    }

    /// Load one settings file. A missing file means all settings use defaults;
    /// malformed TOML and invalid value types are reported to the caller.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read settings file {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("cannot parse settings file {}", path.display()))
    }

    /// Parse one settings document using the same schema at every call site.
    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        toml::from_str(raw).context("invalid server settings")
    }

    /// Load project-local settings from `.membrick/server.toml`.
    pub(crate) fn for_project(project_root: &Path) -> anyhow::Result<Self> {
        Self::load(&project_root.join(".membrick").join("server.toml"))
    }

    pub(crate) fn effective_ignore_paths(&self) -> Vec<String> {
        self.ignore_paths.clone().unwrap_or_else(|| {
            DEFAULT_IGNORE_PATHS
                .iter()
                .map(|path| (*path).to_string())
                .collect()
        })
    }

    pub(crate) fn logging_level(&self) -> Option<&str> {
        self.logging.as_ref()?.level.as_deref()
    }
}
