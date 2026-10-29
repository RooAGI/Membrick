//! Discovery for the CLI process that can host a project resident.

use anyhow::Result;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve the CLI selected by a caller or binding, then look for `membrick`
/// on PATH. The current executable is deliberately never assumed to be the
/// Membrick CLI.
pub(crate) fn resolve_resident_executable(candidate: Option<&Path>) -> Result<PathBuf> {
    resolve_from(candidate, std::env::var_os("PATH").as_deref())
}

fn resolve_from(candidate: Option<&Path>, search_path: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(candidate) = candidate {
        anyhow::ensure!(
            is_executable_file(candidate),
            "configured resident executable is missing or not executable: {}",
            candidate.display()
        );
        return Ok(candidate.to_owned());
    }

    let executable_name = if cfg!(windows) {
        "membrick.exe"
    } else {
        "membrick"
    };
    if let Some(search_path) = search_path {
        for directory in std::env::split_paths(search_path) {
            let candidate = directory.join(executable_name);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    anyhow::bail!(
        "could not find the Membrick CLI; set resident_executable or install `membrick` on PATH"
    )
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "membrick-resident-executable-{}-{}-{id}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_cli(path: &Path) {
        std::fs::write(path, b"test cli").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn explicit_candidate_takes_precedence_over_path() {
        let root = TempDir::new();
        let explicit = root.0.join("explicit");
        let on_path = root.0.join(if cfg!(windows) {
            "membrick.exe"
        } else {
            "membrick"
        });
        write_cli(&explicit);
        write_cli(&on_path);

        let resolved = resolve_from(Some(&explicit), Some(root.0.as_os_str())).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn path_lookup_finds_only_the_membrick_cli_name() {
        let root = TempDir::new();
        let cli = root.0.join(if cfg!(windows) {
            "membrick.exe"
        } else {
            "membrick"
        });
        write_cli(&cli);

        let resolved = resolve_from(None, Some(root.0.as_os_str())).unwrap();
        assert_eq!(resolved, cli);
    }

    #[test]
    fn missing_or_invalid_candidate_does_not_fall_back_to_another_executable() {
        let root = TempDir::new();
        let path_cli = root.0.join(if cfg!(windows) {
            "membrick.exe"
        } else {
            "membrick"
        });
        write_cli(&path_cli);
        let missing = root.0.join("missing-membrick");

        assert!(resolve_from(Some(&missing), Some(root.0.as_os_str())).is_err());
        assert!(resolve_from(None, None).is_err());
    }
}
