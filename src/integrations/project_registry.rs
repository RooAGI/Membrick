use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct RegistryFile {
    #[serde(default = "registry_version")]
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, BTreeSet<String>>,
}

fn registry_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InstalledProject {
    pub path: String,
    pub providers: Vec<String>,
    pub project_state: String,
}

pub(crate) fn registry_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("unable to determine user home directory")?;
    #[cfg(target_os = "macos")]
    let base = home.join("Library").join("Application Support");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Roaming"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Ok(base.join("membrick").join("installed-projects.json"))
}

pub(crate) fn register(root: &Path, provider: &str) -> Result<()> {
    register_at(&registry_path()?, root, provider)
}

pub(crate) fn unregister(root: &Path, provider: &str) -> Result<()> {
    unregister_at(&registry_path()?, root, provider)
}

pub(crate) fn register_at(path: &Path, root: &Path, provider: &str) -> Result<()> {
    let root = canonical_root(root)?;
    update_at(path, |registry| {
        registry
            .projects
            .entry(root)
            .or_default()
            .insert(provider.to_string());
    })
}

pub(crate) fn unregister_at(path: &Path, root: &Path, provider: &str) -> Result<()> {
    let root = canonical_root(root)?;
    unregister_stored_at(path, &root, provider)
}

/// Remove `provider` from a registration keyed by `stored_root`, the canonical
/// path string recorded at install. Unlike `unregister_at`, the folder need not
/// exist any more.
pub(crate) fn unregister_stored_at(path: &Path, stored_root: &str, provider: &str) -> Result<()> {
    update_at(path, |registry| {
        if let Some(providers) = registry.projects.get_mut(stored_root) {
            providers.remove(provider);
            if providers.is_empty() {
                registry.projects.remove(stored_root);
            }
        }
    })
}

pub(crate) fn list() -> Result<Vec<InstalledProject>> {
    list_at(&registry_path()?)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ProjectStatus {
    pub path: String,
    pub provider: String,
    pub installed: bool,
    pub providers: Vec<String>,
    pub project_state: String,
}

pub(crate) fn status(root: &Path, provider: &str) -> Result<ProjectStatus> {
    status_at(&registry_path()?, root, provider)
}

pub(crate) fn status_at(path: &Path, root: &Path, provider: &str) -> Result<ProjectStatus> {
    let canonical = canonical_root(root)?;
    let registry = read_at(path)?;
    let providers = registry
        .projects
        .get(&canonical)
        .cloned()
        .unwrap_or_default();
    let project_path = PathBuf::from(&canonical);
    let project_state = match crate::state_paths::validate_project_root(&project_path) {
        Ok(_) => "ready",
        Err(_) => "not_installed",
    };
    Ok(ProjectStatus {
        path: canonical,
        provider: provider.to_string(),
        installed: providers.contains(provider),
        providers: providers.into_iter().collect(),
        project_state: project_state.to_string(),
    })
}

pub(crate) fn list_at(path: &Path) -> Result<Vec<InstalledProject>> {
    let registry = read_at(path)?;
    registry
        .projects
        .into_iter()
        .map(|(path, providers)| {
            let project_path = PathBuf::from(&path);
            let project_state = match crate::state_paths::validate_project_root(&project_path) {
                Ok(_) => "ready",
                Err(_) if !project_path.exists() => "missing",
                Err(_) => "needs_attention",
            };
            Ok(InstalledProject {
                path,
                providers: providers.into_iter().collect(),
                project_state: project_state.to_string(),
            })
        })
        .collect()
}

fn canonical_root(root: &Path) -> Result<String> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("project path {} does not exist", root.display()))?;
    canonical
        .to_str()
        .map(str::to_owned)
        .with_context(|| format!("project path {} is not valid UTF-8", canonical.display()))
}

fn update_at(path: &Path, update: impl FnOnce(&mut RegistryFile)) -> Result<()> {
    let parent = path.parent().context("registry path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("json.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let mut registry = read_at(path)?;
    registry.version = registry_version();
    update(&mut registry);
    write_atomic(path, &registry)
}

fn read_at(path: &Path) -> Result<RegistryFile> {
    match fs::read(path) {
        Ok(bytes) => {
            let registry: RegistryFile = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid Membrick project registry {}", path.display()))?;
            anyhow::ensure!(
                registry.version == registry_version(),
                "unsupported Membrick project registry version {}",
                registry.version
            );
            Ok(registry)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile {
            version: registry_version(),
            projects: BTreeMap::new(),
        }),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write_atomic(path: &Path, registry: &RegistryFile) -> Result<()> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".installed-projects.{}.{nonce}.{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, registry)?;
        use std::io::Write;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace project registry {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "membrick-project-registry-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn registrations_are_idempotent_and_keep_providers_independent() {
        let root = temp_root();
        crate::state_paths::initialize_project_state(&root).unwrap();
        let registry = root.join("registry.json");
        register_at(&registry, &root, "codex").unwrap();
        register_at(&registry, &root, "codex").unwrap();
        register_at(&registry, &root, "claude").unwrap();

        let projects = list_at(&registry).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].providers, ["claude", "codex"]);
        assert_eq!(projects[0].project_state, "ready");

        unregister_at(&registry, &root, "claude").unwrap();
        let projects = list_at(&registry).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].providers, ["codex"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_last_provider_removes_project_entry() {
        let root = temp_root();
        let registry = root.join("registry.json");
        register_at(&registry, &root, "agy").unwrap();
        unregister_at(&registry, &root, "agy").unwrap();
        assert!(list_at(&registry).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn list_marks_missing_or_invalid_projects_for_attention() {
        let root = temp_root();
        let registry = root.parent().unwrap().join(format!(
            "registry-{}.json",
            root.file_name().unwrap().to_string_lossy()
        ));
        register_at(&registry, &root, "codex").unwrap();
        assert_eq!(
            list_at(&registry).unwrap()[0].project_state,
            "needs_attention"
        );
        crate::state_paths::initialize_project_state(&root).unwrap();
        assert_eq!(list_at(&registry).unwrap()[0].project_state, "ready");
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(list_at(&registry).unwrap()[0].project_state, "missing");
        let _ = fs::remove_file(registry);
    }

    #[test]
    fn status_reports_provider_membership_and_uninitialized_state() {
        let root = temp_root();
        let registry = root.parent().unwrap().join(format!(
            "registry-status-{}.json",
            root.file_name().unwrap().to_string_lossy()
        ));
        register_at(&registry, &root, "codex").unwrap();
        let status = status_at(&registry, &root, "claude").unwrap();
        assert!(!status.installed);
        assert_eq!(status.providers, ["codex"]);
        assert_eq!(status.project_state, "not_installed");
        let _ = fs::remove_file(registry);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_round_trips_unicode_paths() {
        let base = temp_root();
        let root = base.join("项目 🧠");
        fs::create_dir_all(&root).unwrap();
        crate::state_paths::initialize_project_state(&root).unwrap();
        let registry = base.join("registry.json");
        register_at(&registry, &root, "claude").unwrap();
        assert_eq!(
            list_at(&registry).unwrap()[0].path,
            root.canonicalize().unwrap().to_str().unwrap()
        );
        fs::remove_dir_all(base).unwrap();
    }
}
