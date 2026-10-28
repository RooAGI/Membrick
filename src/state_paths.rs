use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Exclusive lifetime ownership of a project's persistent stores.
/// The operating system releases this lock when the process exits or crashes.
pub(crate) struct ProjectOwnerLock {
    _file: fs::File,
}

impl ProjectOwnerLock {
    pub(crate) fn acquire(project_root: &Path) -> anyhow::Result<Self> {
        use std::fs::OpenOptions;
        let runtime = project_root.join(".membrick").join("runtime");
        fs::create_dir_all(&runtime)?;
        let path = runtime.join("workspace-owner.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => anyhow::anyhow!(
                "workspace owner lock is held for project {}",
                project_root.display()
            ),
            fs::TryLockError::Error(error) => {
                anyhow::anyhow!("cannot lock {}: {error}", path.display())
            }
        })?;
        Ok(Self { _file: file })
    }
}

/// A temporary path beside `server.toml` that no other writer, in this process
/// or another, uses at the same time.
fn settings_temporary_path(state: &Path) -> anyhow::Result<PathBuf> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(state.join(format!(
        ".server.toml.{}.{nonce}.{sequence}.tmp",
        std::process::id()
    )))
}

const RESIDENT_HTTP_PORT_START: u16 = 8465;
const RESIDENT_HTTP_PORT_END: u16 = 9999;

/// Initialize project-local Membrick state, install default server settings
/// without replacing settings the user has already created, and record
/// `workspace` as the project root.
pub(crate) fn initialize_project_state(workspace: &Path) -> anyhow::Result<PathBuf> {
    let settings = initialize_default_settings(workspace)?;
    record_install_settings(workspace)?;
    Ok(settings)
}

fn initialize_default_settings(workspace: &Path) -> anyhow::Result<PathBuf> {
    let state = migrate_legacy_state_dir(workspace)?;
    fs::create_dir_all(&state)?;
    let settings = state.join("server.toml");
    if settings.exists() {
        return Ok(settings);
    }

    let port = available_resident_http_port(workspace)?;
    let defaults = format!(
        "# Membrick local server settings. Existing files are preserved by install.\n\
bind = \"127.0.0.1:8080\"\n\
resident_http_bind = \"127.0.0.1:{port}\"\n\
resident_http_auto_port = true\n\
resident_idle_timeout_seconds = 60\n\
dashboard_url = \"http://127.0.0.1:{port}/dashboard\"\n"
    );
    let temporary = settings_temporary_path(&state)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    if let Err(error) = std::io::Write::write_all(&mut file, defaults.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(file);
    match fs::hard_link(&temporary, &settings) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    fs::remove_file(temporary)?;
    Ok(settings)
}

fn preferred_resident_http_port(workspace: &Path) -> u16 {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let count = u32::from(RESIDENT_HTTP_PORT_END - RESIDENT_HTTP_PORT_START) + 1;
    RESIDENT_HTTP_PORT_START + (hash % u64::from(count)) as u16
}

fn available_resident_http_port(workspace: &Path) -> anyhow::Result<u16> {
    let first = preferred_resident_http_port(workspace);
    let count = u32::from(RESIDENT_HTTP_PORT_END - RESIDENT_HTTP_PORT_START) + 1;
    for offset in 0..count {
        let port = RESIDENT_HTTP_PORT_START
            + ((u32::from(first - RESIDENT_HTTP_PORT_START) + offset) % count) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "no available loopback resident HTTP port in {}..={}",
        RESIDENT_HTTP_PORT_START,
        RESIDENT_HTTP_PORT_END
    )
}

/// Move the previous product's workspace state directory to its current name.
/// If both names exist, merge non-conflicting top-level entries and archive
/// colliding legacy entries without overwriting current Membrick data.
pub(crate) fn migrate_legacy_state_dir(workspace: &Path) -> anyhow::Result<PathBuf> {
    let current = workspace.join(".membrick");
    let legacy = workspace.join(".lint-ai");

    match (current.exists(), legacy.exists()) {
        (false, true) => fs::rename(&legacy, &current).map_err(|error| {
            anyhow::anyhow!(
                "could not migrate legacy Membrick state from {} to {}: {error}",
                legacy.display(),
                current.display()
            )
        })?,
        (true, true) => merge_legacy_state(&current, &legacy)?,
        _ => {}
    }

    Ok(current)
}

fn merge_legacy_state(current: &Path, legacy: &Path) -> anyhow::Result<()> {
    let entries = fs::read_dir(legacy)
        .map_err(|error| {
            anyhow::anyhow!("could not read legacy state {}: {error}", legacy.display())
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let conflicts = entries
        .iter()
        .filter(|entry| fs::symlink_metadata(current.join(entry.file_name())).is_ok())
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    let archive = if conflicts.is_empty() {
        None
    } else {
        Some(create_conflict_archive(current)?)
    };

    for entry in entries {
        let name = entry.file_name();
        let target = if conflicts.contains(&name) {
            archive
                .as_ref()
                .expect("conflicts have an archive")
                .join(&name)
        } else {
            current.join(&name)
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(entry.path(), &target).map_err(|error| {
            anyhow::anyhow!(
                "could not preserve legacy state {} at {}: {error}",
                entry.path().display(),
                target.display()
            )
        })?;
    }
    fs::remove_dir(legacy).map_err(|error| {
        anyhow::anyhow!(
            "could not remove emptied legacy state {}: {error}",
            legacy.display()
        )
    })?;
    Ok(())
}

fn create_conflict_archive(current: &Path) -> anyhow::Result<PathBuf> {
    let archive_root = current.join("migration-archive");
    fs::create_dir_all(&archive_root)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for suffix in 0..1000u16 {
        let name = if suffix == 0 {
            format!("lint-ai-{timestamp}")
        } else {
            format!("lint-ai-{timestamp}-{suffix}")
        };
        let path = archive_root.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!(
        "could not allocate a unique legacy-state archive under {}",
        archive_root.display()
    )
}

/// The logging level installers write when a project has none yet.
const DEFAULT_INSTALL_LOG_LEVEL: &str = "info";

/// Record `root` as `project_root` in its `.membrick/server.toml` and make sure
/// a `[logging] level` is set, keeping every other setting and comment, and a
/// level the user already chose. Only installers write `project_root`: it is
/// what makes a folder the one project root of a Membrick install.
pub fn record_install_settings(root: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let root = root
        .canonicalize()
        .with_context(|| format!("project root {} does not exist", root.display()))?;
    let value = root
        .to_str()
        .with_context(|| format!("project root {} is not valid UTF-8", root.display()))?
        .to_owned();
    let state = root.join(".membrick");
    let settings = state.join("server.toml");
    // Concurrent installers serialize the read, edit and replace, so neither
    // loses the other's change or renames the other's temporary file.
    fs::create_dir_all(&state)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("server.toml.lock"))?;
    lock.lock()
        .with_context(|| format!("cannot lock {}", settings.display()))?;
    let text = match fs::read_to_string(&settings) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", settings.display()));
        }
    };
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("cannot parse {}", settings.display()))?;
    let root_recorded =
        document.get("project_root").and_then(|item| item.as_str()) == Some(value.as_str());
    let level_set = document
        .get("logging")
        .and_then(|logging| logging.get("level"))
        .is_some();
    if root_recorded && level_set {
        return Ok(root);
    }
    if !root_recorded {
        document["project_root"] = toml_edit::value(value);
    }
    if !level_set {
        let logging = document
            .entry("logging")
            .or_insert_with(toml_edit::table)
            .as_table_mut()
            .with_context(|| format!("{} [logging] must be a table", settings.display()))?;
        logging["level"] = toml_edit::value(DEFAULT_INSTALL_LOG_LEVEL);
    }
    let temporary = settings_temporary_path(&state)?;
    fs::write(&temporary, document.to_string())?;
    if let Err(error) = fs::rename(&temporary, &settings) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("cannot write {}", settings.display()));
    }
    Ok(root)
}

/// Return the canonical `root` when it is an installed project root: its
/// `.membrick/server.toml` records `project_root` as this same folder. A
/// recorded path that names another folder means the project was moved or
/// copied.
pub(crate) fn validate_project_root(root: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let root = root
        .canonicalize()
        .with_context(|| format!("project root {} does not exist", root.display()))?;
    let settings = root.join(".membrick").join("server.toml");
    let reinstall = format!("run `membrick install <provider> {}`", root.display());
    let text = fs::read_to_string(&settings).with_context(|| {
        format!(
            "{} is not a Membrick project root: {} is missing; {reinstall}",
            root.display(),
            settings.display()
        )
    })?;
    let parsed: toml::Value =
        toml::from_str(&text).with_context(|| format!("cannot parse {}", settings.display()))?;
    let recorded = parsed
        .get("project_root")
        .and_then(toml::Value::as_str)
        .with_context(|| {
            format!(
                "{} does not set project_root; {reinstall}",
                settings.display()
            )
        })?;
    let same = Path::new(recorded)
        .canonicalize()
        .is_ok_and(|recorded| recorded == root);
    anyhow::ensure!(
        same,
        "{} records project_root {recorded} but belongs to {}; the project was moved or copied, {reinstall}",
        settings.display(),
        root.display()
    );
    Ok(root)
}

/// The installed project root that contains `start`, or `None` when no
/// install covers it. The nearest folder with `.membrick/server.toml` decides:
/// it is returned when valid and reported when not, never skipped for a folder
/// further up. Folders inside a `.membrick` directory are never roots, so a
/// stray store left there cannot capture work meant for the project.
pub(crate) fn find_project_root(start: &Path) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context;
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", start.display()))?;
    for candidate in start.ancestors() {
        if candidate
            .components()
            .any(|component| component.as_os_str() == ".membrick")
        {
            continue;
        }
        if candidate.join(".membrick").join("server.toml").is_file() {
            return validate_project_root(candidate).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("membrick-state-paths-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn merges_non_conflicting_legacy_state_and_archives_conflicts() {
        let workspace = temp_workspace();
        let current = workspace.join(".membrick");
        let legacy = workspace.join(".lint-ai");
        fs::create_dir_all(current.join("provider-telemetry")).unwrap();
        fs::create_dir_all(legacy.join("provider-telemetry")).unwrap();
        fs::create_dir_all(legacy.join("memory")).unwrap();
        fs::write(current.join("provider-telemetry/claude.json"), "current").unwrap();
        fs::write(legacy.join("provider-telemetry/claude.json"), "legacy").unwrap();
        fs::write(legacy.join("memory/decision.md"), "decision").unwrap();

        assert_eq!(migrate_legacy_state_dir(&workspace).unwrap(), current);
        assert_eq!(
            fs::read_to_string(current.join("provider-telemetry/claude.json")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(current.join("memory/decision.md")).unwrap(),
            "decision"
        );
        let archived = current.join("migration-archive");
        let archived_legacy = fs::read_dir(archived)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("provider-telemetry/claude.json");
        assert_eq!(fs::read_to_string(archived_legacy).unwrap(), "legacy");
        assert!(!legacy.exists());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn concurrent_installers_create_one_complete_settings_file() {
        let workspace = temp_workspace();
        let first_root = workspace.clone();
        let second_root = workspace.clone();
        let first = std::thread::spawn(move || initialize_project_state(&first_root).unwrap());
        let second = std::thread::spawn(move || initialize_project_state(&second_root).unwrap());
        let first_settings = first.join().unwrap();
        let second_settings = second.join().unwrap();
        assert_eq!(first_settings, second_settings);
        let parsed: toml::Value = fs::read_to_string(&first_settings)
            .unwrap()
            .parse()
            .unwrap();
        assert!(parsed["resident_http_bind"].as_str().is_some());
        assert!(parsed["dashboard_url"].as_str().is_some());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn project_http_port_candidates_are_stable_and_project_specific() {
        let first = temp_workspace();
        let second = temp_workspace();
        let first_port = preferred_resident_http_port(&first);
        assert_eq!(first_port, preferred_resident_http_port(&first));
        assert_ne!(first_port, preferred_resident_http_port(&second));
        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn initializes_default_server_settings_once_and_preserves_custom_settings() {
        let workspace = temp_workspace();
        let settings = initialize_project_state(&workspace).unwrap();
        let raw = fs::read_to_string(&settings).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["bind"].as_str(), Some("127.0.0.1:8080"));
        let bind = parsed["resident_http_bind"].as_str().unwrap();
        let port = bind.strip_prefix("127.0.0.1:").unwrap();
        let port = port.parse::<u16>().unwrap();
        assert!((RESIDENT_HTTP_PORT_START..=RESIDENT_HTTP_PORT_END).contains(&port));
        assert!(parsed["resident_http_auto_port"].as_bool().unwrap());
        assert_eq!(
            parsed["dashboard_url"].as_str(),
            Some(format!("http://127.0.0.1:{port}/dashboard").as_str())
        );

        let custom = r#"resident_http_bind = "127.0.0.1:9999"
"#;
        fs::write(&settings, custom).unwrap();
        initialize_project_state(&workspace).unwrap();
        let reinstalled = fs::read_to_string(&settings).unwrap();
        assert!(reinstalled.starts_with(custom), "{reinstalled}");
        let parsed: toml::Value = toml::from_str(&reinstalled).unwrap();
        assert_eq!(
            parsed["resident_http_bind"].as_str(),
            Some("127.0.0.1:9999")
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    /// A fresh canonical folder, as installers and hooks see it.
    fn temp_root() -> PathBuf {
        temp_workspace().canonicalize().unwrap()
    }

    fn write_settings(root: &Path, text: &str) {
        fs::create_dir_all(root.join(".membrick")).unwrap();
        fs::write(root.join(".membrick/server.toml"), text).unwrap();
    }

    #[test]
    fn install_records_the_project_root() {
        let root = temp_root();
        initialize_project_state(&root).unwrap();
        let parsed: toml::Value =
            toml::from_str(&fs::read_to_string(root.join(".membrick/server.toml")).unwrap())
                .unwrap();
        assert_eq!(parsed["project_root"].as_str(), root.to_str());
        assert_eq!(parsed["logging"]["level"].as_str(), Some("info"));
        assert_eq!(validate_project_root(&root).unwrap(), root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_keeps_a_logging_level_the_user_chose() {
        let root = temp_root();
        write_settings(&root, "[logging]\nlevel = \"debug\"\n");
        initialize_project_state(&root).unwrap();
        let parsed: toml::Value =
            toml::from_str(&fs::read_to_string(root.join(".membrick/server.toml")).unwrap())
                .unwrap();
        assert_eq!(parsed["logging"]["level"].as_str(), Some("debug"));
        assert_eq!(parsed["project_root"].as_str(), root.to_str());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_adds_a_level_to_an_existing_logging_table() {
        let root = temp_root();
        write_settings(&root, "[logging]\n# chosen per machine\n");
        initialize_project_state(&root).unwrap();
        let text = fs::read_to_string(root.join(".membrick/server.toml")).unwrap();
        assert!(text.contains("# chosen per machine"), "{text}");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["logging"]["level"].as_str(), Some("info"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recording_keeps_other_settings_and_comments() {
        let root = temp_root();
        write_settings(
            &root,
            "# Membrick local server settings.\nbind = \"127.0.0.1:8080\"\n\n[logging]\nlevel = \"debug\"\n",
        );
        record_install_settings(&root).unwrap();
        let text = fs::read_to_string(root.join(".membrick/server.toml")).unwrap();
        assert!(text.contains("# Membrick local server settings."), "{text}");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["bind"].as_str(), Some("127.0.0.1:8080"));
        assert_eq!(parsed["project_root"].as_str(), root.to_str());
        assert_eq!(parsed["logging"]["level"].as_str(), Some("debug"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recording_twice_changes_nothing() {
        let root = temp_root();
        record_install_settings(&root).unwrap();
        let first = fs::read_to_string(root.join(".membrick/server.toml")).unwrap();
        record_install_settings(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".membrick/server.toml")).unwrap(),
            first
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_settings_is_not_a_project_root() {
        let root = temp_root();
        let error = validate_project_root(&root).unwrap_err().to_string();
        assert!(error.contains("is not a Membrick project root"), "{error}");
        assert!(error.contains("membrick install"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_without_project_root_are_rejected() {
        let root = temp_root();
        write_settings(&root, "bind = \"127.0.0.1:8080\"\n");
        let error = validate_project_root(&root).unwrap_err().to_string();
        assert!(error.contains("does not set project_root"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_root_naming_another_folder_is_rejected() {
        let root = temp_root();
        let elsewhere = temp_root();
        write_settings(
            &root,
            &format!("project_root = {:?}\n", elsewhere.to_str().unwrap()),
        );
        let error = validate_project_root(&root).unwrap_err().to_string();
        assert!(error.contains("moved or copied"), "{error}");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(elsewhere);
    }

    #[test]
    fn project_root_naming_a_missing_folder_is_rejected() {
        let root = temp_root();
        write_settings(&root, "project_root = \"/no/such/membrick/project\"\n");
        let error = validate_project_root(&root).unwrap_err().to_string();
        assert!(error.contains("moved or copied"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_settings_are_rejected() {
        let root = temp_root();
        write_settings(&root, "project_root = \n");
        let error = validate_project_root(&root).unwrap_err().to_string();
        assert!(error.contains("cannot parse"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_subdirectory_resolves_to_the_installed_root() {
        let root = temp_root();
        record_install_settings(&root).unwrap();
        let nested = root.join("src/integrations");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested).unwrap(), Some(root.clone()));
        assert_eq!(find_project_root(&root).unwrap(), Some(root.clone()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn folders_inside_the_state_directory_resolve_to_the_root() {
        let root = temp_root();
        record_install_settings(&root).unwrap();
        // A stray store left by an earlier hook that took this folder as its
        // root, complete with its own settings, must not capture the session.
        let stray = root.join(".membrick/runtime");
        write_settings(
            &stray,
            &format!("project_root = {:?}\n", stray.to_str().unwrap()),
        );
        let inside = stray.join("provider-leases");
        fs::create_dir_all(&inside).unwrap();
        assert_eq!(find_project_root(&stray).unwrap(), Some(root.clone()));
        assert_eq!(find_project_root(&inside).unwrap(), Some(root.clone()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_folder_with_no_install_has_no_project_root() {
        let folder = temp_root();
        let nested = folder.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested).unwrap(), None);
        // A `.membrick` folder alone does not make a root.
        fs::create_dir_all(folder.join(".membrick/memory")).unwrap();
        assert_eq!(find_project_root(&nested).unwrap(), None);
        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn the_nearest_install_that_is_not_set_up_is_reported() {
        let root = temp_root();
        write_settings(&root, "bind = \"127.0.0.1:8080\"\n");
        let nested = root.join("docs");
        fs::create_dir_all(&nested).unwrap();
        let error = find_project_root(&nested).unwrap_err().to_string();
        assert!(error.contains("does not set project_root"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn non_english_project_paths_record_validate_and_resolve() {
        let base = temp_root();
        for name in ["项目 记忆", "프로젝트", "Ñandú café", "проект", "メモ 🧠"]
        {
            let root = base.join(name);
            fs::create_dir_all(root.join("src/子目录")).unwrap();
            let root = root.canonicalize().unwrap();
            initialize_project_state(&root).unwrap();
            let text = fs::read_to_string(root.join(".membrick/server.toml")).unwrap();
            let parsed: toml::Value = toml::from_str(&text).unwrap();
            assert_eq!(parsed["project_root"].as_str(), root.to_str(), "{name}");
            assert_eq!(validate_project_root(&root).unwrap(), root, "{name}");
            assert_eq!(
                find_project_root(&root.join("src/子目录")).unwrap(),
                Some(root.clone()),
                "{name}"
            );
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_nested_install_is_its_own_project_root() {
        let outer = temp_root();
        record_install_settings(&outer).unwrap();
        let inner = outer.join("vendor/inner");
        fs::create_dir_all(&inner).unwrap();
        record_install_settings(&inner).unwrap();
        let deep = inner.join("lib");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_project_root(&deep).unwrap(), Some(inner.clone()));
        assert_eq!(
            find_project_root(&outer.join("vendor")).unwrap(),
            Some(outer.clone())
        );
        let _ = fs::remove_dir_all(outer);
    }
}
