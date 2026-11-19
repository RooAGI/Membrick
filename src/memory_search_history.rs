//! Private durable snapshots for user-scoped API searches that can be reviewed
//! later by their search ID.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemorySearchHistoryRecord {
    pub search_id: String,
    pub user_id: String,
    pub request: Value,
    pub response: crate::memory_api::SearchResponse,
    pub created_at_ms: u64,
}

pub(crate) fn new_search_id() -> String {
    format!("memory:{}", crate::memory_client::generated_id())
}

pub(crate) fn record_search(
    project_root: &Path,
    record: &MemorySearchHistoryRecord,
) -> anyhow::Result<()> {
    let directory = project_root.join(".membrick").join("memory-search-history");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = record_path(&directory, &record.search_id);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn read_search(
    project_root: &Path,
    search_id: &str,
) -> anyhow::Result<Option<MemorySearchHistoryRecord>> {
    let path = record_path(
        &project_root.join(".membrick").join("memory-search-history"),
        search_id,
    );
    match std::fs::read(path) {
        Ok(bytes) => {
            let record: MemorySearchHistoryRecord = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(record.search_id == search_id, "search ID digest mismatch");
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn record_path(directory: &Path, search_id: &str) -> PathBuf {
    let digest = Sha256::digest(search_id.as_bytes());
    let filename = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    directory.join(format!("{filename}.json"))
}
