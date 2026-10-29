//! Durable, host-owned policy inventory for future downloaded extensions.
//!
//! This store is deliberately not wired to extension activation or the static
//! linkme registry. Its path is supplied by trusted host configuration and
//! must live outside data that extensions can write.

use crate::extension_admission::{
    AdmissionPolicy, ApprovedExtension, AvailableCapability, PermissionGrant,
};
use crate::extension_trust::{TrustPolicy, TrustedPublisherKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const EXTENSION_POLICY_SCHEMA_VERSION: u32 = 3;
const MAX_POLICY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_POLICY_TTL_SECONDS: u64 = 90 * 24 * 60 * 60;
const MAX_AUDIT_EVENTS: usize = 256;
const MAX_AUDIT_ACTOR_BYTES: usize = 128;
const MAX_AUDIT_RATIONALE_BYTES: usize = 1024;
const MAX_AUDIT_ID_BYTES: usize = 256;
const MAX_AUDIT_VERSION_BYTES: usize = 128;

fn system_time_now() -> Result<u64, ExtensionPolicyError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ExtensionPolicyError::ClockBeforeIssue)
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
fn test_audit_context() -> PolicyAuditContext {
    PolicyAuditContext {
        actor: "test-operator".into(),
        rationale: "test policy operation".into(),
    }
}

/// Durable operator-owned trust and approval state.
///
/// `valid_until_unix_seconds` bounds how long an offline host may use this
/// snapshot. An expired snapshot is rejected; `replace` is the explicit path
/// for installing a fresh policy. Revision is assigned by the store and is
/// always advanced on successful replacement.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPolicyInventory {
    pub schema_version: u32,
    pub revision: u64,
    pub issued_at_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
    pub trust_policy: TrustPolicy,
    pub releases: Vec<ExtensionPolicyRelease>,
    pub rollback_floors: Vec<ExtensionVersionFloor>,
    /// Bounded host-owned history. Store mutations ignore replacement-supplied
    /// values and carry forward the persisted ledger themselves.
    pub audit_ledger: PolicyAuditLedger,
}

/// Bounded recent audit history. Older entries are removed in sequence order;
/// `dropped_through_sequence` makes that retention boundary explicit.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyAuditLedger {
    pub next_sequence: u64,
    pub dropped_through_sequence: u64,
    pub events: Vec<PolicyAuditRecord>,
}

impl Default for PolicyAuditLedger {
    fn default() -> Self {
        Self {
            next_sequence: 1,
            dropped_through_sequence: 0,
            events: Vec::new(),
        }
    }
}

/// Caller-provided attribution. The store bounds and persists these strings,
/// but cannot authenticate who supplied them.
#[derive(Clone, Debug)]
pub struct PolicyAuditContext {
    pub actor: String,
    pub rationale: String,
}

/// Lifecycle action recorded in a successful policy snapshot.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditAction {
    PolicyInitialized,
    PolicyRefreshed,
    PublisherKeyAdded,
    PublisherKeyRotated,
    PublisherKeyRevoked,
    PublisherKeyScopeChanged,
    ReleaseApproved,
    ReleaseReapproved,
    ReleaseDisabled,
    ReleaseRevoked,
    GrantSetChanged,
    MinimumVersionChanged,
    RollbackFloorSet,
    RollbackFloorRaised,
    HostApiVersionChanged,
}

/// Only successfully committed operations are currently recorded.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditOutcome {
    Applied,
}

/// Metadata-only lifecycle event. It deliberately has no manifest, artifact,
/// request, or extension payload fields.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyAuditRecord {
    pub sequence: u64,
    pub policy_revision: u64,
    pub timestamp_unix_seconds: u64,
    pub action: PolicyAuditAction,
    pub outcome: PolicyAuditOutcome,
    pub actor: String,
    pub rationale: String,
    pub extension_id: Option<String>,
    pub publisher_id: Option<String>,
    pub key_id: Option<String>,
    pub previous_key_id: Option<String>,
    pub new_key_id: Option<String>,
    pub rotation_from_key_id: Option<String>,
    pub previous_key_fingerprint_sha256: Option<String>,
    pub new_key_fingerprint_sha256: Option<String>,
    pub previous_key_active: Option<bool>,
    pub new_key_active: Option<bool>,
    pub previous_key_allowlist_sha256: Option<String>,
    pub new_key_allowlist_sha256: Option<String>,
    pub previous_host_api_version: Option<String>,
    pub new_host_api_version: Option<String>,
    pub manifest_sha256: Option<String>,
    pub artifact_sha256: Option<String>,
    pub previous_state: Option<ExtensionReleaseState>,
    pub new_state: Option<ExtensionReleaseState>,
    pub previous_grants_sha256: Option<String>,
    pub new_grants_sha256: Option<String>,
    pub previous_minimum_version: Option<String>,
    pub new_minimum_version: Option<String>,
    pub previous_floor: Option<String>,
    pub new_floor: Option<String>,
}

/// One exact release pin and its host-owned policy state.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPolicyRelease {
    pub approved: ApprovedExtension,
    pub state: ExtensionReleaseState,
    /// Operator approval generation for changes to grants or admission gates.
    pub approval_revision: u64,
    /// Human-readable rationale for this approval generation.
    pub approval_reason: String,
}

/// Release state retained by policy even when it must no longer be admitted.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionReleaseState {
    Approved,
    Disabled,
    Revoked,
}

/// Persistent minimum version for one stable extension ID.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionVersionFloor {
    pub extension_id: String,
    pub minimum_version: String,
}

/// Atomic access to one host-configured policy file.
pub struct ExtensionPolicyStore {
    path: PathBuf,
    lock_path: PathBuf,
}

#[derive(Debug)]
pub enum ExtensionPolicyError {
    Io(std::io::Error),
    Missing,
    AlreadyInitialized,
    Corrupt,
    UnsupportedSchema(u32),
    Expired,
    ClockBeforeIssue,
    Invalid(String),
    RevisionConflict { expected: u64, actual: u64 },
    RollbackFloor,
    RevocationReenabled,
    ReapprovalRequired,
    KeyIdentityChanged,
    AuditContextInvalid,
    AuditSequenceExhausted,
}

impl fmt::Display for ExtensionPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "extension policy I/O failed: {error}"),
            Self::Missing => formatter.write_str("extension policy is missing"),
            Self::AlreadyInitialized => formatter.write_str("extension policy already exists"),
            Self::Corrupt => formatter.write_str("extension policy is corrupt or invalid"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported extension policy schema version {version}"
                )
            }
            Self::Expired => formatter.write_str("extension policy has expired"),
            Self::ClockBeforeIssue => {
                formatter.write_str("system clock is earlier than extension policy issue time")
            }
            Self::Invalid(message) => write!(formatter, "invalid extension policy: {message}"),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "extension policy revision conflict: expected {expected}, found {actual}"
            ),
            Self::RollbackFloor => formatter.write_str("extension rollback floor cannot decrease"),
            Self::RevocationReenabled => formatter
                .write_str("revoked extension identity or signing key cannot be re-enabled"),
            Self::ReapprovalRequired => formatter.write_str(
                "release grant or admission changes require an explicit approval revision and reason",
            ),
            Self::KeyIdentityChanged => formatter.write_str(
                "publisher key IDs cannot be removed or rebound to different key material",
            ),
            Self::AuditContextInvalid => formatter.write_str(
                "extension policy audit actor and rationale must be non-empty, bounded, and contain no control characters",
            ),
            Self::AuditSequenceExhausted => {
                formatter.write_str("extension policy audit sequence is exhausted")
            }
        }
    }
}

impl std::error::Error for ExtensionPolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ExtensionPolicyError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl ExtensionPolicyInventory {
    /// Create schema-v2 policy data. The store assigns durable revision and
    /// audit history when it commits the inventory.
    pub fn new(
        issued_at_unix_seconds: u64,
        valid_until_unix_seconds: u64,
        trust_policy: TrustPolicy,
        releases: Vec<ExtensionPolicyRelease>,
        rollback_floors: Vec<ExtensionVersionFloor>,
    ) -> Self {
        Self {
            schema_version: EXTENSION_POLICY_SCHEMA_VERSION,
            revision: 1,
            issued_at_unix_seconds,
            valid_until_unix_seconds,
            trust_policy,
            releases,
            rollback_floors,
            audit_ledger: PolicyAuditLedger::default(),
        }
    }

    fn trust_policy(&self) -> TrustPolicy {
        self.trust_policy.clone()
    }

    /// Convert approved, enabled releases into the existing admission API.
    /// Runtime capability availability and active IDs remain caller supplied.
    fn admission_policy(
        &self,
        available_capabilities: Vec<AvailableCapability>,
        active_extension_ids: Vec<String>,
    ) -> Result<AdmissionPolicy, ExtensionPolicyError> {
        let floors = floor_map(&self.rollback_floors)?;
        let mut approved_extensions = Vec::new();
        for release in &self.releases {
            if release.state != ExtensionReleaseState::Approved {
                continue;
            }
            let mut approved = release.approved.clone();
            if let Some(floor) = floors.get(&approved.extension_id) {
                let current = Version::parse(&approved.minimum_version)
                    .map_err(|_| ExtensionPolicyError::Corrupt)?;
                let persistent =
                    Version::parse(floor).map_err(|_| ExtensionPolicyError::Corrupt)?;
                if persistent > current {
                    approved.minimum_version = floor.clone();
                }
            }
            approved_extensions.push(approved);
        }
        Ok(AdmissionPolicy {
            host_api_version: self.trust_policy.host_api_version.clone(),
            approved_extensions,
            available_capabilities,
            active_extension_ids,
        })
    }
}

impl ExtensionPolicyStore {
    /// Open an existing policy file. Missing, corrupt, unsupported, or stale
    /// policy fails closed. `path` must come from trusted host configuration.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ExtensionPolicyError> {
        let store = Self::from_path(path.into());
        store.load()?;
        Ok(store)
    }

    /// Time-injected equivalent of [`Self::open`], available only to tests.
    #[cfg(test)]
    fn open_at(
        path: impl Into<PathBuf>,
        now_unix_seconds: u64,
    ) -> Result<Self, ExtensionPolicyError> {
        let path = path.into();
        let store = Self::from_path(path);
        store.load_at(now_unix_seconds)?;
        Ok(store)
    }

    /// Explicitly initialize a policy file using the system wall clock. It
    /// never creates a permissive default or overwrites existing policy.
    pub fn initialize(
        path: impl Into<PathBuf>,
        inventory: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
    ) -> Result<Self, ExtensionPolicyError> {
        Self::initialize_with_time(path, inventory, audit_context, system_time_now()?)
    }

    #[cfg(test)]
    fn initialize_at(
        path: impl Into<PathBuf>,
        inventory: ExtensionPolicyInventory,
        now_unix_seconds: u64,
    ) -> Result<Self, ExtensionPolicyError> {
        Self::initialize_with_time(path, inventory, test_audit_context(), now_unix_seconds)
    }

    #[cfg(test)]
    fn initialize_at_with_context(
        path: impl Into<PathBuf>,
        inventory: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
        now_unix_seconds: u64,
    ) -> Result<Self, ExtensionPolicyError> {
        Self::initialize_with_time(path, inventory, audit_context, now_unix_seconds)
    }

    fn initialize_with_time(
        path: impl Into<PathBuf>,
        mut inventory: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
        now_unix_seconds: u64,
    ) -> Result<Self, ExtensionPolicyError> {
        validate_audit_context(&audit_context)?;
        let store = Self::from_path(path.into());
        let _lock = store.lock()?;
        match fs::symlink_metadata(&store.path) {
            Ok(_) => return Err(ExtensionPolicyError::AlreadyInitialized),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ExtensionPolicyError::Io(error)),
        }
        inventory.revision = 1;
        inventory.audit_ledger = PolicyAuditLedger::default();
        validate_inventory(&inventory, now_unix_seconds)?;
        let events = initialization_audit_events(&inventory, now_unix_seconds, &audit_context);
        append_audit_events(
            &mut inventory.audit_ledger,
            events,
            inventory.revision,
            now_unix_seconds,
            &audit_context,
        )?;
        store.write_atomic(&inventory)?;
        Ok(store)
    }

    fn from_path(path: PathBuf) -> Self {
        let lock_path = path.with_extension("policy.lock");
        Self { path, lock_path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read and validate current state using the system clock.
    pub fn load(&self) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        self.load_with_time(system_time_now()?)
    }

    /// Deterministic time-injected load available only to tests.
    #[cfg(test)]
    fn load_at(
        &self,
        now_unix_seconds: u64,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        self.load_with_time(now_unix_seconds)
    }

    fn load_with_time(
        &self,
        now_unix_seconds: u64,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        let inventory = self.read_inventory()?;
        validate_inventory(&inventory, now_unix_seconds)?;
        Ok(inventory)
    }

    pub fn trust_policy(&self) -> Result<TrustPolicy, ExtensionPolicyError> {
        Ok(self.load()?.trust_policy())
    }

    pub fn admission_policy(
        &self,
        available_capabilities: Vec<AvailableCapability>,
        active_extension_ids: Vec<String>,
    ) -> Result<AdmissionPolicy, ExtensionPolicyError> {
        self.load()?
            .admission_policy(available_capabilities, active_extension_ids)
    }

    #[cfg(test)]
    fn trust_policy_at(&self, now_unix_seconds: u64) -> Result<TrustPolicy, ExtensionPolicyError> {
        Ok(self.load_at(now_unix_seconds)?.trust_policy())
    }

    #[cfg(test)]
    fn admission_policy_at(
        &self,
        now_unix_seconds: u64,
        available_capabilities: Vec<AvailableCapability>,
        active_extension_ids: Vec<String>,
    ) -> Result<AdmissionPolicy, ExtensionPolicyError> {
        self.load_at(now_unix_seconds)?
            .admission_policy(available_capabilities, active_extension_ids)
    }

    /// Replace policy with an optimistic revision check and an atomic rename.
    /// An expired but parseable policy may be explicitly refreshed this way;
    /// malformed or unsupported state must be repaired by the operator first.
    pub fn replace(
        &self,
        expected_revision: u64,
        replacement: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        self.replace_with_time(
            expected_revision,
            replacement,
            audit_context,
            system_time_now()?,
        )
    }

    #[cfg(test)]
    fn replace_at(
        &self,
        expected_revision: u64,
        replacement: ExtensionPolicyInventory,
        now_unix_seconds: u64,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        self.replace_with_time(
            expected_revision,
            replacement,
            test_audit_context(),
            now_unix_seconds,
        )
    }

    #[cfg(test)]
    fn replace_at_with_context(
        &self,
        expected_revision: u64,
        replacement: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
        now_unix_seconds: u64,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        self.replace_with_time(
            expected_revision,
            replacement,
            audit_context,
            now_unix_seconds,
        )
    }

    fn replace_with_time(
        &self,
        expected_revision: u64,
        mut replacement: ExtensionPolicyInventory,
        audit_context: PolicyAuditContext,
        now_unix_seconds: u64,
    ) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        validate_audit_context(&audit_context)?;
        let _lock = self.lock()?;
        let current = self.read_inventory()?;
        validate_structure(&current)?;
        if current.revision != expected_revision {
            return Err(ExtensionPolicyError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        // The caller supplies policy decisions, never audit history. Always
        // carry forward the persisted ledger before validating or generating
        // records for this transition.
        replacement.audit_ledger = current.audit_ledger.clone();
        replacement.revision = current.revision;
        validate_inventory(&replacement, now_unix_seconds)?;
        validate_transition(&current, &replacement)?;
        replacement.revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| ExtensionPolicyError::Invalid("revision overflow".into()))?;
        let events =
            transition_audit_events(&current, &replacement, now_unix_seconds, &audit_context);
        append_audit_events(
            &mut replacement.audit_ledger,
            events,
            replacement.revision,
            now_unix_seconds,
            &audit_context,
        )?;
        self.write_atomic(&replacement)?;
        Ok(replacement)
    }

    fn read_inventory(&self) -> Result<ExtensionPolicyInventory, ExtensionPolicyError> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ExtensionPolicyError::Missing
            } else {
                ExtensionPolicyError::Io(error)
            }
        })?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_POLICY_BYTES {
            return Err(ExtensionPolicyError::Corrupt);
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&self.path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(ExtensionPolicyError::Corrupt);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::take(file, MAX_POLICY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(ExtensionPolicyError::Corrupt);
        }
        serde_json::from_slice(&bytes).map_err(|_| ExtensionPolicyError::Corrupt)
    }

    fn lock(&self) -> Result<File, ExtensionPolicyError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&self.lock_path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(ExtensionPolicyError::Corrupt);
        }
        file.lock()?;
        Ok(file)
    }

    fn write_atomic(
        &self,
        inventory: &ExtensionPolicyInventory,
    ) -> Result<(), ExtensionPolicyError> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| ExtensionPolicyError::Invalid("policy path has no file name".into()))?;
        let nonce = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            nonce
        ));
        let bytes =
            serde_json::to_vec_pretty(inventory).map_err(|_| ExtensionPolicyError::Corrupt)?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(ExtensionPolicyError::Invalid(
                "policy exceeds size limit".into(),
            ));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<(), ExtensionPolicyError> {
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn validate_inventory(
    inventory: &ExtensionPolicyInventory,
    now_unix_seconds: u64,
) -> Result<(), ExtensionPolicyError> {
    validate_structure(inventory)?;
    if now_unix_seconds < inventory.issued_at_unix_seconds {
        return Err(ExtensionPolicyError::ClockBeforeIssue);
    }
    if now_unix_seconds >= inventory.valid_until_unix_seconds {
        return Err(ExtensionPolicyError::Expired);
    }
    Ok(())
}

fn validate_structure(inventory: &ExtensionPolicyInventory) -> Result<(), ExtensionPolicyError> {
    if inventory.schema_version != EXTENSION_POLICY_SCHEMA_VERSION {
        return Err(ExtensionPolicyError::UnsupportedSchema(
            inventory.schema_version,
        ));
    }
    if inventory.revision == 0
        || inventory.valid_until_unix_seconds <= inventory.issued_at_unix_seconds
        || inventory.valid_until_unix_seconds - inventory.issued_at_unix_seconds
            > MAX_POLICY_TTL_SECONDS
    {
        return Err(ExtensionPolicyError::Invalid(
            "revision or policy validity interval is invalid".into(),
        ));
    }
    if inventory.trust_policy.host_api_version.len() > MAX_AUDIT_VERSION_BYTES
        || Version::parse(&inventory.trust_policy.host_api_version).is_err()
    {
        return Err(ExtensionPolicyError::Invalid(
            "host API version must be bounded SemVer".into(),
        ));
    }
    validate_audit_ledger(&inventory.audit_ledger, inventory.revision)?;
    validate_keys(&inventory.trust_policy.publisher_keys)?;
    let floors = floor_map(&inventory.rollback_floors)?;
    let mut exact_releases = BTreeSet::new();
    let mut approved_ids = BTreeSet::new();
    let mut publisher_by_extension = BTreeMap::<&str, &str>::new();
    for release in &inventory.releases {
        let approved = &release.approved;
        if release.approval_revision == 0
            || release.approval_reason.trim().is_empty()
            || approved.extension_id.trim().is_empty()
            || approved.publisher_id.trim().is_empty()
            || approved.publisher_key_id.trim().is_empty()
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        if publisher_by_extension
            .insert(&approved.extension_id, &approved.publisher_id)
            .is_some_and(|publisher| publisher != approved.publisher_id)
        {
            return Err(ExtensionPolicyError::Invalid(
                "an extension ID cannot be transferred to another publisher".into(),
            ));
        }
        let key = (
            approved.extension_id.as_str(),
            approved.manifest_sha256.as_str(),
            approved.artifact_sha256.as_str(),
        );
        if !exact_releases.insert(key) {
            return Err(ExtensionPolicyError::Corrupt);
        }
        if release.state == ExtensionReleaseState::Approved
            && !approved_ids.insert(approved.extension_id.as_str())
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        let version =
            Version::parse(&approved.version).map_err(|_| ExtensionPolicyError::Corrupt)?;
        let minimum =
            Version::parse(&approved.minimum_version).map_err(|_| ExtensionPolicyError::Corrupt)?;
        if version < minimum
            || !is_sha256(&approved.manifest_sha256)
            || !is_sha256(&approved.artifact_sha256)
            || approved.target.trim().is_empty()
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        let mut capabilities = BTreeSet::new();
        if approved
            .provides
            .iter()
            .any(|id| id.trim().is_empty() || !capabilities.insert(id.as_str()))
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        if release.state == ExtensionReleaseState::Approved {
            if let Some(floor) = floors.get(&approved.extension_id) {
                let floor = Version::parse(floor).map_err(|_| ExtensionPolicyError::Corrupt)?;
                if version < floor {
                    return Err(ExtensionPolicyError::RollbackFloor);
                }
            }
            if !inventory.trust_policy.publisher_keys.iter().any(|key| {
                key.publisher_id == approved.publisher_id
                    && key.key_id == approved.publisher_key_id
                    && key.active
                    && key
                        .allowed_extension_ids
                        .iter()
                        .any(|id| id == &approved.extension_id)
            }) {
                return Err(ExtensionPolicyError::Corrupt);
            }
        }
        validate_grants(&approved.grants)?;
    }
    Ok(())
}

fn validate_keys(keys: &[TrustedPublisherKey]) -> Result<(), ExtensionPolicyError> {
    let mut identities = BTreeSet::new();
    let mut public_key_material = BTreeSet::new();
    for key in keys {
        if !is_bounded_audit_id(&key.publisher_id)
            || !is_bounded_audit_id(&key.key_id)
            || key.public_key_hex.len() != 64
            || key.allowed_extension_ids.is_empty()
            || !identities.insert((key.publisher_id.as_str(), key.key_id.as_str()))
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        let mut allowed = BTreeSet::new();
        if key
            .allowed_extension_ids
            .iter()
            .any(|id| !is_bounded_audit_id(id) || !allowed.insert(id.as_str()))
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        if !key
            .public_key_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        // A revoked public key must not be relabeled under a different
        // publisher/key identity. Hex is case-insensitive, so compare a
        // canonical representation of the material.
        if !public_key_material.insert(key.public_key_hex.to_ascii_lowercase()) {
            return Err(ExtensionPolicyError::Corrupt);
        }
    }
    Ok(())
}

fn validate_grants(grants: &[PermissionGrant]) -> Result<(), ExtensionPolicyError> {
    let mut unique = BTreeSet::new();
    if grants.iter().any(|grant| {
        grant.permission.trim().is_empty() || grant.scope.trim().is_empty() || !unique.insert(grant)
    }) {
        return Err(ExtensionPolicyError::Corrupt);
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn is_bounded_audit_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_AUDIT_ID_BYTES
        && !value.chars().any(char::is_control)
}

fn floor_map(
    floors: &[ExtensionVersionFloor],
) -> Result<BTreeMap<String, String>, ExtensionPolicyError> {
    let mut map = BTreeMap::new();
    for floor in floors {
        if floor.extension_id.trim().is_empty()
            || Version::parse(&floor.minimum_version).is_err()
            || map
                .insert(floor.extension_id.clone(), floor.minimum_version.clone())
                .is_some()
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
    }
    Ok(map)
}

fn validate_audit_context(context: &PolicyAuditContext) -> Result<(), ExtensionPolicyError> {
    if context.actor.trim().is_empty()
        || context.actor.len() > MAX_AUDIT_ACTOR_BYTES
        || context.rationale.trim().is_empty()
        || context.rationale.len() > MAX_AUDIT_RATIONALE_BYTES
        || context.actor.chars().any(char::is_control)
        || context.rationale.chars().any(char::is_control)
    {
        return Err(ExtensionPolicyError::AuditContextInvalid);
    }
    Ok(())
}

fn validate_audit_ledger(
    ledger: &PolicyAuditLedger,
    policy_revision: u64,
) -> Result<(), ExtensionPolicyError> {
    if ledger.next_sequence == 0
        || ledger.dropped_through_sequence >= ledger.next_sequence
        || ledger.events.len() > MAX_AUDIT_EVENTS
    {
        return Err(ExtensionPolicyError::Corrupt);
    }
    let mut expected = ledger
        .dropped_through_sequence
        .checked_add(1)
        .ok_or(ExtensionPolicyError::Corrupt)?;
    for event in &ledger.events {
        if event.sequence != expected
            || event.policy_revision == 0
            || event.policy_revision > policy_revision
            || event.outcome != PolicyAuditOutcome::Applied
            || event.actor.trim().is_empty()
            || event.actor.len() > MAX_AUDIT_ACTOR_BYTES
            || event.rationale.trim().is_empty()
            || event.rationale.len() > MAX_AUDIT_RATIONALE_BYTES
            || event.actor.chars().any(char::is_control)
            || event.rationale.chars().any(char::is_control)
            || event
                .key_id
                .as_deref()
                .is_some_and(|id| !is_bounded_audit_id(id))
            || event
                .previous_key_id
                .as_deref()
                .is_some_and(|id| !is_bounded_audit_id(id))
            || event
                .new_key_id
                .as_deref()
                .is_some_and(|id| !is_bounded_audit_id(id))
            || event
                .rotation_from_key_id
                .as_deref()
                .is_some_and(|id| !is_bounded_audit_id(id))
            || event
                .publisher_id
                .as_deref()
                .is_some_and(|id| !is_bounded_audit_id(id))
            || event
                .previous_host_api_version
                .as_deref()
                .is_some_and(|version| {
                    version.len() > MAX_AUDIT_VERSION_BYTES || Version::parse(version).is_err()
                })
            || event
                .new_host_api_version
                .as_deref()
                .is_some_and(|version| {
                    version.len() > MAX_AUDIT_VERSION_BYTES || Version::parse(version).is_err()
                })
            || event
                .manifest_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .artifact_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .previous_grants_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .new_grants_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .previous_key_fingerprint_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .new_key_fingerprint_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .previous_key_allowlist_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || event
                .new_key_allowlist_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || !valid_key_audit_details(event)
            || (event.action == PolicyAuditAction::HostApiVersionChanged
                && (event.previous_host_api_version.is_none()
                    || event.new_host_api_version.is_none()
                    || event.previous_host_api_version == event.new_host_api_version))
        {
            return Err(ExtensionPolicyError::Corrupt);
        }
        expected = expected
            .checked_add(1)
            .ok_or(ExtensionPolicyError::Corrupt)?;
    }
    if expected != ledger.next_sequence {
        return Err(ExtensionPolicyError::Corrupt);
    }
    Ok(())
}

fn valid_key_audit_details(event: &PolicyAuditRecord) -> bool {
    let has_key_details = event.publisher_id.is_some()
        && event.key_id.is_some()
        && event.previous_key_fingerprint_sha256.is_some()
        && event.new_key_fingerprint_sha256.is_some()
        && event.previous_key_active.is_some()
        && event.new_key_active.is_some()
        && event.previous_key_allowlist_sha256.is_some()
        && event.new_key_allowlist_sha256.is_some();
    match event.action {
        PolicyAuditAction::PublisherKeyAdded => {
            event.publisher_id.is_some()
                && event.key_id.is_some()
                && event.key_id == event.new_key_id
                && event.previous_key_id.is_none()
                && event.previous_key_fingerprint_sha256.is_none()
                && event.previous_key_active.is_none()
                && event.previous_key_allowlist_sha256.is_none()
                && event.new_key_id.is_some()
                && event.new_key_fingerprint_sha256.is_some()
                && event.new_key_active.is_some()
                && event.new_key_allowlist_sha256.is_some()
                && event.rotation_from_key_id.is_none()
        }
        PolicyAuditAction::PublisherKeyRotated => {
            has_key_details
                && event.key_id == event.new_key_id
                && event.previous_key_id.is_some()
                && event.new_key_id.is_some()
                && event.previous_key_id != event.new_key_id
                && event.previous_key_active == Some(true)
                && event.new_key_active == Some(true)
                && event.previous_key_fingerprint_sha256 != event.new_key_fingerprint_sha256
                && event.rotation_from_key_id == event.previous_key_id
        }
        PolicyAuditAction::PublisherKeyRevoked => {
            has_key_details
                && event.key_id == event.new_key_id
                && event.previous_key_id == event.new_key_id
                && event.previous_key_active == Some(true)
                && event.new_key_active == Some(false)
                && event.rotation_from_key_id.is_none()
        }
        PolicyAuditAction::PublisherKeyScopeChanged => {
            has_key_details
                && event.key_id == event.new_key_id
                && event.previous_key_id == event.new_key_id
                && event.previous_key_allowlist_sha256 != event.new_key_allowlist_sha256
                && event.rotation_from_key_id.is_none()
        }
        _ => true,
    }
}

fn append_audit_events(
    ledger: &mut PolicyAuditLedger,
    events: Vec<PolicyAuditRecord>,
    policy_revision: u64,
    timestamp_unix_seconds: u64,
    context: &PolicyAuditContext,
) -> Result<(), ExtensionPolicyError> {
    let first_sequence = ledger.next_sequence;
    let event_count =
        u64::try_from(events.len()).map_err(|_| ExtensionPolicyError::AuditSequenceExhausted)?;
    let next_sequence = first_sequence
        .checked_add(event_count)
        .ok_or(ExtensionPolicyError::AuditSequenceExhausted)?;
    for (index, mut event) in events.into_iter().enumerate() {
        let sequence = first_sequence
            .checked_add(index as u64)
            .ok_or(ExtensionPolicyError::AuditSequenceExhausted)?;
        event.sequence = sequence;
        event.policy_revision = policy_revision;
        event.timestamp_unix_seconds = timestamp_unix_seconds;
        event.outcome = PolicyAuditOutcome::Applied;
        event.actor.clone_from(&context.actor);
        event.rationale.clone_from(&context.rationale);
        ledger.events.push(event);
    }
    if ledger.events.len() > MAX_AUDIT_EVENTS {
        let drop_count = ledger.events.len() - MAX_AUDIT_EVENTS;
        ledger.dropped_through_sequence = ledger.events[drop_count - 1].sequence;
        ledger.events.drain(..drop_count);
    }
    ledger.next_sequence = next_sequence;
    validate_audit_ledger(ledger, policy_revision)
}

fn empty_audit_event(action: PolicyAuditAction) -> PolicyAuditRecord {
    PolicyAuditRecord {
        sequence: 0,
        policy_revision: 0,
        timestamp_unix_seconds: 0,
        action,
        outcome: PolicyAuditOutcome::Applied,
        actor: String::new(),
        rationale: String::new(),
        extension_id: None,
        publisher_id: None,
        key_id: None,
        previous_key_id: None,
        new_key_id: None,
        rotation_from_key_id: None,
        previous_key_fingerprint_sha256: None,
        new_key_fingerprint_sha256: None,
        previous_key_active: None,
        new_key_active: None,
        previous_key_allowlist_sha256: None,
        new_key_allowlist_sha256: None,
        previous_host_api_version: None,
        new_host_api_version: None,
        manifest_sha256: None,
        artifact_sha256: None,
        previous_state: None,
        new_state: None,
        previous_grants_sha256: None,
        new_grants_sha256: None,
        previous_minimum_version: None,
        new_minimum_version: None,
        previous_floor: None,
        new_floor: None,
    }
}

fn release_audit_event(
    action: PolicyAuditAction,
    release: &ExtensionPolicyRelease,
) -> PolicyAuditRecord {
    let approved = &release.approved;
    let mut event = empty_audit_event(action);
    event.extension_id = Some(approved.extension_id.clone());
    event.publisher_id = Some(approved.publisher_id.clone());
    event.key_id = Some(approved.publisher_key_id.clone());
    event.manifest_sha256 = Some(approved.manifest_sha256.clone());
    event.artifact_sha256 = Some(approved.artifact_sha256.clone());
    event.new_state = Some(release.state);
    event.new_grants_sha256 = Some(grants_digest(&approved.grants));
    event.new_minimum_version = Some(approved.minimum_version.clone());
    event
}

fn grants_digest(grants: &[PermissionGrant]) -> String {
    let mut sorted = grants.to_vec();
    sorted.sort();
    let bytes = serde_json::to_vec(&sorted).expect("PermissionGrant serialization is infallible");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn public_key_fingerprint(key: &TrustedPublisherKey) -> String {
    let bytes = (0..32)
        .map(|index| {
            u8::from_str_radix(&key.public_key_hex[index * 2..index * 2 + 2], 16)
                .expect("validated public key hex")
        })
        .collect::<Vec<_>>();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn key_allowlist_digest(key: &TrustedPublisherKey) -> String {
    let mut sorted = key.allowed_extension_ids.iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    let bytes = serde_json::to_vec(&sorted).expect("extension ID serialization is infallible");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn key_audit_event(
    action: PolicyAuditAction,
    previous: Option<&TrustedPublisherKey>,
    new: Option<&TrustedPublisherKey>,
) -> PolicyAuditRecord {
    let key = new.or(previous).expect("key audit event has a key");
    let mut event = empty_audit_event(action);
    event.publisher_id = Some(key.publisher_id.clone());
    event.key_id = Some(key.key_id.clone());
    event.previous_key_id = previous.map(|key| key.key_id.clone());
    event.new_key_id = new.map(|key| key.key_id.clone());
    event.previous_key_fingerprint_sha256 = previous.map(public_key_fingerprint);
    event.new_key_fingerprint_sha256 = new.map(public_key_fingerprint);
    event.previous_key_active = previous.map(|key| key.active);
    event.new_key_active = new.map(|key| key.active);
    event.previous_key_allowlist_sha256 = previous.map(key_allowlist_digest);
    event.new_key_allowlist_sha256 = new.map(key_allowlist_digest);
    event
}

fn initialization_audit_events(
    inventory: &ExtensionPolicyInventory,
    _timestamp_unix_seconds: u64,
    _context: &PolicyAuditContext,
) -> Vec<PolicyAuditRecord> {
    let mut events = vec![empty_audit_event(PolicyAuditAction::PolicyInitialized)];
    for key in &inventory.trust_policy.publisher_keys {
        events.push(key_audit_event(
            PolicyAuditAction::PublisherKeyAdded,
            None,
            Some(key),
        ));
    }
    for release in &inventory.releases {
        let action = match release.state {
            ExtensionReleaseState::Approved => PolicyAuditAction::ReleaseApproved,
            ExtensionReleaseState::Disabled => PolicyAuditAction::ReleaseDisabled,
            ExtensionReleaseState::Revoked => PolicyAuditAction::ReleaseRevoked,
        };
        events.push(release_audit_event(action, release));
    }
    for floor in &inventory.rollback_floors {
        let mut event = empty_audit_event(PolicyAuditAction::RollbackFloorSet);
        event.extension_id = Some(floor.extension_id.clone());
        event.new_floor = Some(floor.minimum_version.clone());
        events.push(event);
    }
    events
}

fn transition_audit_events(
    current: &ExtensionPolicyInventory,
    replacement: &ExtensionPolicyInventory,
    _timestamp_unix_seconds: u64,
    _context: &PolicyAuditContext,
) -> Vec<PolicyAuditRecord> {
    let mut events = Vec::new();
    for new_key in &replacement.trust_policy.publisher_keys {
        let previous =
            current.trust_policy.publisher_keys.iter().find(|key| {
                key.publisher_id == new_key.publisher_id && key.key_id == new_key.key_id
            });
        match previous {
            None => {
                let rotated_from = current
                    .trust_policy
                    .publisher_keys
                    .iter()
                    .filter(|old_key| {
                        old_key.publisher_id == new_key.publisher_id
                            && old_key.active
                            && new_key.active
                    })
                    .filter(|old_key| {
                        replacement
                            .trust_policy
                            .publisher_keys
                            .iter()
                            .any(|candidate| {
                                candidate.publisher_id == old_key.publisher_id
                                    && candidate.key_id == old_key.key_id
                                    && !candidate.active
                            })
                    })
                    .collect::<Vec<_>>();
                let previous = if rotated_from.len() == 1 {
                    Some(rotated_from[0])
                } else {
                    None
                };
                let mut event = key_audit_event(
                    if previous.is_some() {
                        PolicyAuditAction::PublisherKeyRotated
                    } else {
                        PolicyAuditAction::PublisherKeyAdded
                    },
                    previous,
                    Some(new_key),
                );
                event.key_id = Some(new_key.key_id.clone());
                event.rotation_from_key_id = previous.map(|key| key.key_id.clone());
                events.push(event);
            }
            Some(old_key) if old_key.active && !new_key.active => {
                let event = key_audit_event(
                    PolicyAuditAction::PublisherKeyRevoked,
                    Some(old_key),
                    Some(new_key),
                );
                events.push(event);
            }
            Some(old_key) if key_allowlist_digest(old_key) != key_allowlist_digest(new_key) => {
                let event = key_audit_event(
                    PolicyAuditAction::PublisherKeyScopeChanged,
                    Some(old_key),
                    Some(new_key),
                );
                events.push(event);
            }
            _ => {}
        }
    }

    for new_release in &replacement.releases {
        let approved = &new_release.approved;
        let old_release = current.releases.iter().find(|old| {
            old.approved.extension_id == approved.extension_id
                && old.approved.manifest_sha256 == approved.manifest_sha256
                && old.approved.artifact_sha256 == approved.artifact_sha256
        });
        let Some(old_release) = old_release else {
            let action = match new_release.state {
                ExtensionReleaseState::Approved => PolicyAuditAction::ReleaseApproved,
                ExtensionReleaseState::Disabled => PolicyAuditAction::ReleaseDisabled,
                ExtensionReleaseState::Revoked => PolicyAuditAction::ReleaseRevoked,
            };
            events.push(release_audit_event(action, new_release));
            continue;
        };
        let old = &old_release.approved;
        if old_release.state != new_release.state {
            let action = match new_release.state {
                ExtensionReleaseState::Approved => PolicyAuditAction::ReleaseReapproved,
                ExtensionReleaseState::Disabled => PolicyAuditAction::ReleaseDisabled,
                ExtensionReleaseState::Revoked => PolicyAuditAction::ReleaseRevoked,
            };
            let mut event = release_audit_event(action, new_release);
            event.previous_state = Some(old_release.state);
            events.push(event);
        }
        if old_release.approval_revision != new_release.approval_revision
            && new_release.state == ExtensionReleaseState::Approved
            && !(old_release.state != ExtensionReleaseState::Approved
                && new_release.state == ExtensionReleaseState::Approved)
        {
            let mut event = release_audit_event(PolicyAuditAction::ReleaseReapproved, new_release);
            event.previous_state = Some(old_release.state);
            event.previous_grants_sha256 = Some(grants_digest(&old.grants));
            event.previous_minimum_version = Some(old.minimum_version.clone());
            events.push(event);
        }
        if old.grants != approved.grants {
            let mut event = release_audit_event(PolicyAuditAction::GrantSetChanged, new_release);
            event.previous_state = Some(old_release.state);
            event.previous_grants_sha256 = Some(grants_digest(&old.grants));
            events.push(event);
        }
        if old.minimum_version != approved.minimum_version {
            let mut event =
                release_audit_event(PolicyAuditAction::MinimumVersionChanged, new_release);
            event.previous_state = Some(old_release.state);
            event.previous_minimum_version = Some(old.minimum_version.clone());
            events.push(event);
        }
    }

    let old_floors = floor_map(&current.rollback_floors).unwrap_or_default();
    let new_floors = floor_map(&replacement.rollback_floors).unwrap_or_default();
    for (extension_id, new_floor) in new_floors {
        let old_floor = old_floors.get(&extension_id);
        if old_floor.is_none_or(|old| old != &new_floor) {
            let mut event = empty_audit_event(if old_floor.is_some() {
                PolicyAuditAction::RollbackFloorRaised
            } else {
                PolicyAuditAction::RollbackFloorSet
            });
            event.extension_id = Some(extension_id);
            event.previous_floor = old_floor.cloned();
            event.new_floor = Some(new_floor);
            events.push(event);
        }
    }
    if current.trust_policy.host_api_version != replacement.trust_policy.host_api_version {
        let mut event = empty_audit_event(PolicyAuditAction::HostApiVersionChanged);
        event.previous_host_api_version = Some(current.trust_policy.host_api_version.clone());
        event.new_host_api_version = Some(replacement.trust_policy.host_api_version.clone());
        events.push(event);
    }
    if events.is_empty() {
        events.push(empty_audit_event(PolicyAuditAction::PolicyRefreshed));
    }
    events
}

fn validate_transition(
    current: &ExtensionPolicyInventory,
    replacement: &ExtensionPolicyInventory,
) -> Result<(), ExtensionPolicyError> {
    if replacement.issued_at_unix_seconds < current.issued_at_unix_seconds {
        return Err(ExtensionPolicyError::ClockBeforeIssue);
    }
    let old_floors = floor_map(&current.rollback_floors)?;
    let new_floors = floor_map(&replacement.rollback_floors)?;
    for (extension_id, old_floor) in old_floors {
        let new_floor = new_floors
            .get(&extension_id)
            .ok_or(ExtensionPolicyError::RollbackFloor)?;
        if Version::parse(new_floor).map_err(|_| ExtensionPolicyError::Corrupt)?
            < Version::parse(&old_floor).map_err(|_| ExtensionPolicyError::Corrupt)?
        {
            return Err(ExtensionPolicyError::RollbackFloor);
        }
    }

    for old_key in &current.trust_policy.publisher_keys {
        let new_key =
            replacement.trust_policy.publisher_keys.iter().find(|key| {
                key.publisher_id == old_key.publisher_id && key.key_id == old_key.key_id
            });
        let Some(new_key) = new_key else {
            return Err(ExtensionPolicyError::KeyIdentityChanged);
        };
        if new_key.public_key_hex != old_key.public_key_hex {
            return Err(ExtensionPolicyError::KeyIdentityChanged);
        }
        if !old_key.active && new_key.active {
            return Err(ExtensionPolicyError::RevocationReenabled);
        }
    }

    for old_release in &current.releases {
        let old = &old_release.approved;
        let new_release = replacement.releases.iter().find(|release| {
            let new = &release.approved;
            new.extension_id == old.extension_id
                && new.manifest_sha256 == old.manifest_sha256
                && new.artifact_sha256 == old.artifact_sha256
        });
        let Some(new_release) = new_release else {
            return Err(ExtensionPolicyError::Invalid(
                "release records cannot be removed from the policy inventory".into(),
            ));
        };
        let new = &new_release.approved;
        if new.version != old.version
            || new.publisher_id != old.publisher_id
            || new.publisher_key_id != old.publisher_key_id
            || new.target != old.target
            || new.provides != old.provides
        {
            return Err(ExtensionPolicyError::Invalid(
                "signed release identity claims cannot change in place".into(),
            ));
        }
        if old_release.state == ExtensionReleaseState::Revoked
            && new_release.state != ExtensionReleaseState::Revoked
        {
            return Err(ExtensionPolicyError::RevocationReenabled);
        }
        let approval_changed = old.minimum_version != new.minimum_version
            || old.grants != new.grants
            || (old_release.state != ExtensionReleaseState::Approved
                && new_release.state == ExtensionReleaseState::Approved);
        if approval_changed {
            if old_release.approval_revision.checked_add(1) != Some(new_release.approval_revision)
                || new_release.approval_reason.trim().is_empty()
            {
                return Err(ExtensionPolicyError::ReapprovalRequired);
            }
        } else if new_release.approval_revision != old_release.approval_revision
            || new_release.approval_reason != old_release.approval_reason
        {
            return Err(ExtensionPolicyError::ReapprovalRequired);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_trust::TrustPolicy;

    const NOW: u64 = 1_800_000_000;

    fn fixture() -> ExtensionPolicyInventory {
        ExtensionPolicyInventory::new(
            NOW,
            NOW + 30 * 24 * 60 * 60,
            TrustPolicy {
                host_api_version: "0.1.0".into(),
                publisher_keys: vec![TrustedPublisherKey {
                    publisher_id: "publisher.example".into(),
                    key_id: "key-1".into(),
                    public_key_hex: "11".repeat(32),
                    allowed_extension_ids: vec!["org.example.plugin".into()],
                    active: true,
                }],
            },
            vec![ExtensionPolicyRelease {
                approved: ApprovedExtension {
                    extension_id: "org.example.plugin".into(),
                    version: "2.3.0".into(),
                    minimum_version: "2.2.0".into(),
                    manifest_sha256: format!("sha256:{}", "22".repeat(32)),
                    publisher_id: "publisher.example".into(),
                    publisher_key_id: "key-1".into(),
                    artifact_sha256: format!("sha256:{}", "33".repeat(32)),
                    target: "wasm32-unknown-unknown".into(),
                    provides: vec!["example.capability.v1".into()],
                    grants: vec![PermissionGrant {
                        permission: "documents.read".into(),
                        scope: "owner_workspace".into(),
                    }],
                },
                state: ExtensionReleaseState::Approved,
                approval_revision: 1,
                approval_reason: "initial operator approval".into(),
            }],
            vec![ExtensionVersionFloor {
                extension_id: "org.example.plugin".into(),
                minimum_version: "2.2.0".into(),
            }],
        )
    }

    fn path() -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "membrick-extension-policy-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_files(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("policy.lock"));
        if let Some(parent) = path.parent() {
            let prefix = format!(".{}.", path.file_name().unwrap().to_string_lossy());
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    fn initialized(path: &Path) -> ExtensionPolicyStore {
        ExtensionPolicyStore::initialize_at(path, fixture(), NOW).unwrap()
    }

    #[test]
    fn policy_survives_restart_with_exact_pins_and_grants() {
        let path = path();
        let store = initialized(&path);
        drop(store);
        let loaded = ExtensionPolicyStore::open_at(&path, NOW)
            .unwrap()
            .load_at(NOW)
            .unwrap();
        assert_eq!(loaded.revision, 1);
        assert!(loaded.audit_ledger.events.iter().any(|event| {
            event.action == PolicyAuditAction::PolicyInitialized
                && event.outcome == PolicyAuditOutcome::Applied
        }));
        assert!(loaded
            .audit_ledger
            .events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1));
        assert_eq!(
            loaded.releases[0].approved.manifest_sha256,
            fixture().releases[0].approved.manifest_sha256
        );
        assert_eq!(
            loaded.releases[0].approved.artifact_sha256,
            fixture().releases[0].approved.artifact_sha256
        );
        assert_eq!(loaded.releases[0].approved.publisher_key_id, "key-1");
        assert_eq!(
            loaded.releases[0].approved.grants[0].permission,
            "documents.read"
        );
        remove_files(&path);
    }

    #[test]
    fn missing_corrupt_unsupported_and_expired_policy_fail_closed() {
        let path = path();
        assert!(matches!(
            ExtensionPolicyStore::open(&path),
            Err(ExtensionPolicyError::Missing)
        ));
        let store = initialized(&path);
        assert!(matches!(
            store.load_at(NOW + 31 * 24 * 60 * 60),
            Err(ExtensionPolicyError::Expired)
        ));
        fs::write(&path, b"{broken").unwrap();
        assert!(matches!(
            store.load_at(NOW),
            Err(ExtensionPolicyError::Corrupt)
        ));
        let mut future = fixture();
        future.schema_version = EXTENSION_POLICY_SCHEMA_VERSION + 1;
        fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(matches!(
            store.load_at(NOW),
            Err(ExtensionPolicyError::UnsupportedSchema(_))
        ));
        let mut legacy = fixture();
        legacy.schema_version = 1;
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(matches!(
            store.load_at(NOW),
            Err(ExtensionPolicyError::UnsupportedSchema(1))
        ));
        let mut previous_schema = fixture();
        previous_schema.schema_version = 2;
        fs::write(&path, serde_json::to_vec(&previous_schema).unwrap()).unwrap();
        assert!(matches!(
            store.load_at(NOW),
            Err(ExtensionPolicyError::UnsupportedSchema(2))
        ));
        remove_files(&path);
    }

    #[test]
    fn initialization_records_bounded_caller_attribution_and_metadata_only_fields() {
        let path = path();
        let context = PolicyAuditContext {
            actor: "operator:alice".into(),
            rationale: "reviewed publisher and release pins".into(),
        };
        let store =
            ExtensionPolicyStore::initialize_at_with_context(&path, fixture(), context, NOW)
                .unwrap();
        let ledger = &store.load_at(NOW).unwrap().audit_ledger;
        assert!(ledger.events.iter().all(|event| {
            event.actor == "operator:alice"
                && event.rationale == "reviewed publisher and release pins"
                && event.outcome == PolicyAuditOutcome::Applied
        }));
        assert!(ledger.events.iter().any(|event| {
            event.action == PolicyAuditAction::ReleaseApproved
                && event.manifest_sha256.as_deref() == Some(&format!("sha256:{}", "22".repeat(32)))
                && event.artifact_sha256.as_deref() == Some(&format!("sha256:{}", "33".repeat(32)))
                && event.new_grants_sha256.is_some()
        }));
        let key_event = ledger
            .events
            .iter()
            .find(|event| event.action == PolicyAuditAction::PublisherKeyAdded)
            .unwrap();
        assert_eq!(key_event.previous_key_id, None);
        assert_eq!(key_event.new_key_id.as_deref(), Some("key-1"));
        assert_eq!(key_event.new_key_active, Some(true));
        assert!(key_event.new_key_fingerprint_sha256.is_some());
        assert!(key_event.new_key_allowlist_sha256.is_some());
        let encoded = serde_json::to_string(ledger).unwrap();
        assert!(!encoded.contains("ACTUAL_ARTIFACT_PRIVATE_CONTENT"));
        assert!(!encoded.contains(&"11".repeat(32)));
        assert!(!serde_json::to_string(key_event)
            .unwrap()
            .contains("org.example.plugin"));
        remove_files(&path);
    }

    #[test]
    fn key_scope_widening_and_narrowing_record_sorted_allowlist_digests() {
        let path = path();
        let store = initialized(&path);
        let original = store.load_at(NOW).unwrap().trust_policy.publisher_keys[0].clone();

        let mut widened = fixture();
        widened.issued_at_unix_seconds = NOW + 1;
        widened.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        widened.trust_policy.publisher_keys[0]
            .allowed_extension_ids
            .push("org.example.second".into());
        store.replace_at(1, widened, NOW + 1).unwrap();
        let after_widen = store.load_at(NOW + 1).unwrap();
        let widened_key = &after_widen.trust_policy.publisher_keys[0];
        let widen_event = after_widen
            .audit_ledger
            .events
            .iter()
            .find(|event| {
                event.policy_revision == 2
                    && event.action == PolicyAuditAction::PublisherKeyScopeChanged
            })
            .unwrap();
        assert_eq!(
            widen_event.previous_key_allowlist_sha256.as_deref(),
            Some(key_allowlist_digest(&original).as_str())
        );
        assert_eq!(
            widen_event.new_key_allowlist_sha256.as_deref(),
            Some(key_allowlist_digest(widened_key).as_str())
        );
        assert_ne!(
            widen_event.previous_key_allowlist_sha256,
            widen_event.new_key_allowlist_sha256
        );
        let widened_event_json = serde_json::to_string(widen_event).unwrap();
        assert!(!widened_event_json.contains("org.example.second"));
        assert!(!widened_event_json.contains(&"11".repeat(32)));
        let widened_allowlist_digest = key_allowlist_digest(widened_key);

        let mut narrowed = after_widen;
        narrowed.issued_at_unix_seconds = NOW + 2;
        narrowed.valid_until_unix_seconds = NOW + 32 * 24 * 60 * 60;
        narrowed.trust_policy.publisher_keys[0].allowed_extension_ids =
            original.allowed_extension_ids.clone();
        store.replace_at(2, narrowed, NOW + 2).unwrap();
        let after_narrow = store.load_at(NOW + 2).unwrap();
        let narrow_event = after_narrow
            .audit_ledger
            .events
            .iter()
            .find(|event| {
                event.policy_revision == 3
                    && event.action == PolicyAuditAction::PublisherKeyScopeChanged
            })
            .unwrap();
        assert_eq!(
            narrow_event.previous_key_allowlist_sha256.as_deref(),
            Some(widened_allowlist_digest.as_str())
        );
        assert_eq!(
            narrow_event.new_key_allowlist_sha256.as_deref(),
            Some(key_allowlist_digest(&original).as_str())
        );
        remove_files(&path);
    }

    #[test]
    fn host_api_version_change_records_bounded_old_and_new_versions() {
        let path = path();
        let store = initialized(&path);
        let mut changed = fixture();
        changed.issued_at_unix_seconds = NOW + 1;
        changed.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        changed.trust_policy.host_api_version = "0.2.0".into();
        store.replace_at(1, changed, NOW + 1).unwrap();
        let persisted = store.load_at(NOW + 1).unwrap();
        let event = persisted
            .audit_ledger
            .events
            .iter()
            .find(|event| event.action == PolicyAuditAction::HostApiVersionChanged)
            .unwrap();
        assert_eq!(event.previous_host_api_version.as_deref(), Some("0.1.0"));
        assert_eq!(event.new_host_api_version.as_deref(), Some("0.2.0"));
        remove_files(&path);
    }

    #[test]
    fn audit_actor_and_rationale_are_bounded() {
        assert!(validate_audit_context(&PolicyAuditContext {
            actor: "operator".into(),
            rationale: " ".into(),
        })
        .is_err());
        assert!(validate_audit_context(&PolicyAuditContext {
            actor: "operator\nforged".into(),
            rationale: "approved".into(),
        })
        .is_err());
        assert!(validate_audit_context(&PolicyAuditContext {
            actor: "x".repeat(MAX_AUDIT_ACTOR_BYTES + 1),
            rationale: "approved".into(),
        })
        .is_err());
        assert!(validate_audit_context(&PolicyAuditContext {
            actor: "operator".into(),
            rationale: "x".repeat(MAX_AUDIT_RATIONALE_BYTES + 1),
        })
        .is_err());
    }

    #[test]
    fn audit_history_is_bounded_and_reports_the_dropped_through_sequence() {
        let context = test_audit_context();
        let event_count = MAX_AUDIT_EVENTS + 17;
        let events = vec![empty_audit_event(PolicyAuditAction::PolicyRefreshed); event_count];
        let mut ledger = PolicyAuditLedger::default();
        append_audit_events(&mut ledger, events, 2, NOW, &context).unwrap();
        assert_eq!(ledger.events.len(), MAX_AUDIT_EVENTS);
        assert_eq!(ledger.dropped_through_sequence, 17);
        assert_eq!(ledger.events.first().unwrap().sequence, 18);
        assert_eq!(ledger.events.last().unwrap().sequence, event_count as u64);
        assert_eq!(ledger.next_sequence, (event_count + 1) as u64);
        validate_audit_ledger(&ledger, 2).unwrap();
    }

    #[test]
    fn corrupted_audit_history_fails_closed() {
        let path = path();
        let _store = initialized(&path);
        let mut inventory: ExtensionPolicyInventory =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        inventory.audit_ledger.events[0].sequence += 1;
        fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(matches!(
            ExtensionPolicyStore::open_at(&path, NOW),
            Err(ExtensionPolicyError::Corrupt)
        ));
        remove_files(&path);
    }

    #[test]
    fn malformed_key_audit_details_fail_closed() {
        let path = path();
        let initialized = initialized(&path);
        let inventory = initialized.load_at(NOW).unwrap();
        remove_files(&path);
        let added = inventory
            .audit_ledger
            .events
            .iter()
            .find(|event| event.action == PolicyAuditAction::PublisherKeyAdded)
            .unwrap()
            .clone();
        for mutate in [
            Box::new(|event: &mut PolicyAuditRecord| event.publisher_id = None)
                as Box<dyn Fn(&mut PolicyAuditRecord)>,
            Box::new(|event: &mut PolicyAuditRecord| event.key_id = None),
            Box::new(|event: &mut PolicyAuditRecord| event.key_id = Some("wrong-key".into())),
        ] {
            let mut event = added.clone();
            mutate(&mut event);
            let ledger = malformed_single_event_ledger(event);
            assert!(validate_audit_ledger(&ledger, 1).is_err());
        }

        let old = TrustedPublisherKey {
            publisher_id: "publisher.example".into(),
            key_id: "key-old".into(),
            public_key_hex: "11".repeat(32),
            allowed_extension_ids: vec!["org.example.plugin".into()],
            active: true,
        };
        let new = TrustedPublisherKey {
            publisher_id: "publisher.example".into(),
            key_id: "key-new".into(),
            public_key_hex: "22".repeat(32),
            allowed_extension_ids: vec!["org.example.plugin".into()],
            active: true,
        };

        let mut rotation = key_audit_event(
            PolicyAuditAction::PublisherKeyRotated,
            Some(&old),
            Some(&new),
        );
        rotation.rotation_from_key_id = Some(old.key_id.clone());
        for mutate in [
            Box::new(|event: &mut PolicyAuditRecord| event.key_id = Some("key-old".into()))
                as Box<dyn Fn(&mut PolicyAuditRecord)>,
            Box::new(|event: &mut PolicyAuditRecord| event.previous_key_active = Some(false)),
            Box::new(|event: &mut PolicyAuditRecord| {
                event.new_key_fingerprint_sha256 = event.previous_key_fingerprint_sha256.clone()
            }),
        ] {
            let mut event = rotation.clone();
            mutate(&mut event);
            let ledger = malformed_single_event_ledger(event);
            assert!(validate_audit_ledger(&ledger, 1).is_err());
        }

        for action in [
            PolicyAuditAction::PublisherKeyRevoked,
            PolicyAuditAction::PublisherKeyScopeChanged,
        ] {
            let inactive_or_changed = TrustedPublisherKey {
                active: action == PolicyAuditAction::PublisherKeyScopeChanged,
                allowed_extension_ids: vec!["org.example.other".into()],
                ..old.clone()
            };
            let mut event = key_audit_event(action, Some(&old), Some(&inactive_or_changed));
            event.key_id = Some("unrelated-key".into());
            let ledger = malformed_single_event_ledger(event);
            assert!(validate_audit_ledger(&ledger, 1).is_err());
        }
    }

    fn malformed_single_event_ledger(mut event: PolicyAuditRecord) -> PolicyAuditLedger {
        let context = test_audit_context();
        event.sequence = 1;
        event.policy_revision = 1;
        event.timestamp_unix_seconds = NOW;
        event.outcome = PolicyAuditOutcome::Applied;
        event.actor = context.actor;
        event.rationale = context.rationale;
        PolicyAuditLedger {
            next_sequence: 2,
            dropped_through_sequence: 0,
            events: vec![event],
        }
    }

    #[test]
    fn duplicate_public_key_material_cannot_be_reintroduced_under_new_id() {
        let path = path();
        let store = initialized(&path);
        let mut rotate = fixture();
        rotate.issued_at_unix_seconds = NOW + 1;
        rotate.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        rotate.trust_policy.publisher_keys[0].active = false;
        rotate
            .trust_policy
            .publisher_keys
            .push(TrustedPublisherKey {
                publisher_id: "publisher.example".into(),
                key_id: "key-2".into(),
                public_key_hex: "44".repeat(32),
                allowed_extension_ids: vec!["org.example.plugin".into()],
                active: true,
            });
        rotate.releases[0].state = ExtensionReleaseState::Disabled;
        store.replace_at(1, rotate, NOW + 1).unwrap();

        let mut reintroduced = store.load_at(NOW + 1).unwrap();
        reintroduced.issued_at_unix_seconds = NOW + 2;
        reintroduced.valid_until_unix_seconds = NOW + 32 * 24 * 60 * 60;
        reintroduced
            .trust_policy
            .publisher_keys
            .push(TrustedPublisherKey {
                publisher_id: "publisher.example".into(),
                key_id: "key-3".into(),
                // Reuse the revoked key-1 material with different hex casing to
                // prove the check compares key bytes, not the string spelling.
                public_key_hex: "11".repeat(32).to_ascii_uppercase(),
                allowed_extension_ids: vec!["org.example.plugin".into()],
                active: true,
            });
        assert!(matches!(
            store.replace_at(2, reintroduced, NOW + 2),
            Err(ExtensionPolicyError::Corrupt)
        ));
        assert_eq!(store.load_at(NOW + 1).unwrap().revision, 2);
        remove_files(&path);
    }

    #[test]
    fn oversized_audit_and_policy_update_preserves_previous_snapshot() {
        let path = path();
        let store = initialized(&path);
        let before_bytes = fs::read(&path).unwrap();
        let before_ledger = store.load_at(NOW).unwrap().audit_ledger;
        let mut oversized = fixture();
        oversized.issued_at_unix_seconds = NOW + 1;
        oversized.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        oversized.releases[0].approved.grants.push(PermissionGrant {
            permission: "p".repeat(MAX_POLICY_BYTES as usize),
            scope: "owner_workspace".into(),
        });
        oversized.releases[0].approval_revision = 2;
        oversized.releases[0].approval_reason = "approved oversized grant".into();
        assert!(matches!(
            store.replace_at(1, oversized, NOW + 1),
            Err(ExtensionPolicyError::Invalid(message))
                if message.contains("exceeds size limit")
        ));
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        assert_eq!(
            store.load_at(NOW).unwrap().audit_ledger.events.len(),
            before_ledger.events.len()
        );
        assert_eq!(store.load_at(NOW).unwrap().revision, 1);
        remove_files(&path);
    }

    #[cfg(unix)]
    #[test]
    fn policy_and_lock_symlinks_fail_closed() {
        use std::os::unix::fs::symlink;

        let target = path();
        let target_store = initialized(&target);
        let alias = path();
        symlink(&target, &alias).unwrap();
        assert!(ExtensionPolicyStore::open_at(&alias, NOW).is_err());

        let lock_alias = path();
        let lock_target = path().with_extension("policy.lock");
        fs::write(&lock_target, b"lock target").unwrap();
        symlink(&lock_target, lock_alias.with_extension("policy.lock")).unwrap();
        assert!(ExtensionPolicyStore::initialize_at(lock_alias.clone(), fixture(), NOW).is_err());

        fs::remove_file(&alias).unwrap();
        fs::remove_file(lock_alias.with_extension("policy.lock")).unwrap();
        fs::remove_file(lock_target).unwrap();
        remove_files(&target);
        drop(target_store);
    }

    #[test]
    fn key_material_cannot_change_under_existing_key_identity() {
        let path = path();
        let store = initialized(&path);
        let mut changed = fixture();
        changed.trust_policy.publisher_keys[0].public_key_hex = "44".repeat(32);
        changed.issued_at_unix_seconds = NOW + 1;
        changed.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        assert!(matches!(
            store.replace_at(1, changed, NOW + 1),
            Err(ExtensionPolicyError::KeyIdentityChanged)
        ));
        assert_eq!(store.load_at(NOW).unwrap().revision, 1);
        remove_files(&path);
    }

    #[test]
    fn key_identity_cannot_be_removed_then_rebound() {
        let path = path();
        let store = initialized(&path);
        let mut removed = fixture();
        removed.trust_policy.publisher_keys.clear();
        removed.issued_at_unix_seconds = NOW + 1;
        removed.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        assert!(store.replace_at(1, removed, NOW + 1).is_err());

        let mut rebound = fixture();
        rebound.trust_policy.publisher_keys[0].public_key_hex = "55".repeat(32);
        rebound.issued_at_unix_seconds = NOW + 1;
        rebound.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        assert!(matches!(
            store.replace_at(1, rebound, NOW + 1),
            Err(ExtensionPolicyError::KeyIdentityChanged)
        ));
        assert_eq!(store.load_at(NOW).unwrap().revision, 1);
        remove_files(&path);
    }

    #[test]
    fn key_rotation_uses_a_new_key_id_and_keeps_old_key_tombstone() {
        let path = path();
        let store = initialized(&path);
        let mut replacement = fixture();
        replacement.issued_at_unix_seconds = NOW + 1;
        replacement.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        replacement.trust_policy.publisher_keys[0].active = false;
        replacement
            .trust_policy
            .publisher_keys
            .push(TrustedPublisherKey {
                publisher_id: "publisher.example".into(),
                key_id: "key-2".into(),
                public_key_hex: "44".repeat(32),
                allowed_extension_ids: vec!["org.example.plugin".into()],
                active: true,
            });
        replacement.releases[0].state = ExtensionReleaseState::Disabled;
        let mut next = replacement.releases[0].clone();
        next.approved.version = "2.4.0".into();
        next.approved.publisher_key_id = "key-2".into();
        next.approved.manifest_sha256 = format!("sha256:{}", "66".repeat(32));
        next.approved.artifact_sha256 = format!("sha256:{}", "77".repeat(32));
        next.state = ExtensionReleaseState::Approved;
        next.approval_revision = 1;
        next.approval_reason = "approved release under rotated key".into();
        replacement.releases.push(next);

        store.replace_at(1, replacement, NOW + 1).unwrap();
        let loaded = store.load_at(NOW + 1).unwrap();
        assert_eq!(loaded.trust_policy.publisher_keys.len(), 2);
        assert!(!loaded.trust_policy.publisher_keys[0].active);
        assert_eq!(
            loaded.trust_policy.publisher_keys[0].public_key_hex,
            "11".repeat(32)
        );
        assert_eq!(loaded.releases[1].approved.publisher_key_id, "key-2");
        let rotation = loaded
            .audit_ledger
            .events
            .iter()
            .find(|event| event.action == PolicyAuditAction::PublisherKeyRotated)
            .unwrap();
        assert_eq!(rotation.rotation_from_key_id.as_deref(), Some("key-1"));
        assert_eq!(rotation.previous_key_id.as_deref(), Some("key-1"));
        assert_eq!(rotation.new_key_id.as_deref(), Some("key-2"));
        assert_eq!(rotation.previous_key_active, Some(true));
        assert_eq!(rotation.new_key_active, Some(true));
        assert!(rotation.previous_key_fingerprint_sha256.is_some());
        assert!(rotation.new_key_fingerprint_sha256.is_some());
        assert_ne!(
            rotation.previous_key_fingerprint_sha256,
            rotation.new_key_fingerprint_sha256
        );
        assert!(loaded.audit_ledger.events.iter().any(|event| {
            event.action == PolicyAuditAction::PublisherKeyRevoked
                && event.key_id.as_deref() == Some("key-1")
                && event.previous_key_active == Some(true)
                && event.new_key_active == Some(false)
        }));

        // A stale, inactive key alone does not make a later independent key
        // introduction look like a rotation.
        let mut independent_add = loaded.clone();
        independent_add.issued_at_unix_seconds = NOW + 2;
        independent_add.valid_until_unix_seconds = NOW + 32 * 24 * 60 * 60;
        independent_add
            .trust_policy
            .publisher_keys
            .push(TrustedPublisherKey {
                publisher_id: "publisher.example".into(),
                key_id: "key-3".into(),
                public_key_hex: "55".repeat(32),
                allowed_extension_ids: vec!["org.example.plugin".into()],
                active: true,
            });
        store.replace_at(2, independent_add, NOW + 2).unwrap();
        let after_add = store.load_at(NOW + 2).unwrap();
        assert!(after_add.audit_ledger.events.iter().any(|event| {
            event.policy_revision == 3
                && event.action == PolicyAuditAction::PublisherKeyAdded
                && event.new_key_id.as_deref() == Some("key-3")
                && event.rotation_from_key_id.is_none()
        }));
        remove_files(&path);
    }

    #[test]
    fn extension_publisher_cannot_transfer_after_old_release_is_disabled() {
        let path = path();
        let store = initialized(&path);
        let mut replacement = fixture();
        replacement.issued_at_unix_seconds = NOW + 1;
        replacement.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        replacement.releases[0].state = ExtensionReleaseState::Disabled;
        replacement
            .trust_policy
            .publisher_keys
            .push(TrustedPublisherKey {
                publisher_id: "other-publisher.example".into(),
                key_id: "key-other".into(),
                public_key_hex: "44".repeat(32),
                allowed_extension_ids: vec!["org.example.plugin".into()],
                active: true,
            });
        let mut transferred = replacement.releases[0].clone();
        transferred.approved.publisher_id = "other-publisher.example".into();
        transferred.approved.publisher_key_id = "key-other".into();
        transferred.approved.version = "3.0.0".into();
        transferred.approved.manifest_sha256 = format!("sha256:{}", "66".repeat(32));
        transferred.approved.artifact_sha256 = format!("sha256:{}", "77".repeat(32));
        transferred.state = ExtensionReleaseState::Approved;
        transferred.approval_revision = 1;
        transferred.approval_reason = "new publisher transfer".into();
        replacement.releases.push(transferred);
        assert!(matches!(
            store.replace_at(1, replacement, NOW + 1),
            Err(ExtensionPolicyError::Invalid(message))
                if message.contains("cannot be transferred")
        ));
        assert_eq!(store.load_at(NOW).unwrap().revision, 1);
        remove_files(&path);
    }

    #[test]
    fn revoked_key_and_release_persist_and_are_excluded_from_admission() {
        let path = path();
        let store = initialized(&path);
        let mut revoked = fixture();
        revoked.trust_policy.publisher_keys[0].active = false;
        revoked.releases[0].state = ExtensionReleaseState::Revoked;
        let saved = store.replace_at(1, revoked, NOW + 1).unwrap();
        assert_eq!(saved.revision, 2);
        drop(store);
        let reopened = ExtensionPolicyStore::open_at(&path, NOW + 1).unwrap();
        assert!(!reopened.trust_policy_at(NOW + 1).unwrap().publisher_keys[0].active);
        assert!(reopened
            .admission_policy_at(NOW + 1, Vec::new(), Vec::new())
            .unwrap()
            .approved_extensions
            .is_empty());
        remove_files(&path);
    }

    #[test]
    fn revoked_release_cannot_be_reenabled_and_disabled_release_is_not_admitted() {
        let policy_path = path();
        let store = initialized(&policy_path);
        let mut revoked = fixture();
        revoked.trust_policy.publisher_keys[0].active = false;
        revoked.releases[0].state = ExtensionReleaseState::Revoked;
        store.replace_at(1, revoked, NOW + 1).unwrap();
        let revoked_bytes = fs::read(&policy_path).unwrap();

        let mut reenabled = fixture();
        reenabled.issued_at_unix_seconds = NOW + 2;
        reenabled.valid_until_unix_seconds = NOW + 32 * 24 * 60 * 60;
        assert!(matches!(
            store.replace_at(2, reenabled, NOW + 2),
            Err(ExtensionPolicyError::RevocationReenabled)
        ));
        assert_eq!(fs::read(&policy_path).unwrap(), revoked_bytes);

        let disabled_path = path();
        let disabled_store = initialized(&disabled_path);
        let mut disabled = fixture();
        disabled.issued_at_unix_seconds = NOW + 1;
        disabled.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        disabled.releases[0].state = ExtensionReleaseState::Disabled;
        disabled_store.replace_at(1, disabled, NOW + 1).unwrap();
        let admission = disabled_store
            .admission_policy_at(NOW + 2, Vec::new(), Vec::new())
            .unwrap();
        assert!(admission.approved_extensions.is_empty());
        assert_eq!(
            disabled_store.load_at(NOW + 2).unwrap().releases[0].state,
            ExtensionReleaseState::Disabled
        );
        remove_files(&policy_path);
        remove_files(&disabled_path);
    }

    #[test]
    fn grant_changes_require_explicit_reapproval() {
        let path = path();
        let store = initialized(&path);
        let mut updated = fixture();
        updated.releases[0].approved.grants.push(PermissionGrant {
            permission: "documents.write".into(),
            scope: "owner_workspace".into(),
        });
        let original = fs::read(&path).unwrap();
        assert!(matches!(
            store.replace_at(1, updated.clone(), NOW + 1),
            Err(ExtensionPolicyError::ReapprovalRequired)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);

        updated.releases[0].approval_revision = 2;
        updated.releases[0].approval_reason = "operator approved documents.write".into();
        // The caller does not get to replace or clear prior audit history.
        updated.audit_ledger = PolicyAuditLedger::default();
        store.replace_at(1, updated, NOW + 1).unwrap();
        let persisted = store.load_at(NOW + 1).unwrap();
        assert!(persisted.audit_ledger.events.len() > 1);
        assert!(persisted.audit_ledger.events.iter().any(|event| {
            event.action == PolicyAuditAction::GrantSetChanged
                && event.policy_revision == 2
                && event.actor == test_audit_context().actor
                && event.rationale == "test policy operation"
                && event.outcome == PolicyAuditOutcome::Applied
                && event.previous_grants_sha256.is_some()
                && event.new_grants_sha256.is_some()
        }));
        let policy = store
            .admission_policy_at(NOW + 1, Vec::new(), Vec::new())
            .unwrap();
        assert_eq!(policy.approved_extensions[0].grants.len(), 2);
        remove_files(&path);
    }

    #[test]
    fn lowering_minimum_version_requires_explicit_reapproval() {
        let path = path();
        let store = initialized(&path);
        let mut updated = fixture();
        updated.releases[0].approved.minimum_version = "2.1.0".into();
        assert!(matches!(
            store.replace_at(1, updated.clone(), NOW + 1),
            Err(ExtensionPolicyError::ReapprovalRequired)
        ));
        updated.releases[0].approval_revision = 2;
        updated.releases[0].approval_reason = "operator lowered version gate".into();
        store.replace_at(1, updated, NOW + 1).unwrap();
        remove_files(&path);
    }

    #[test]
    fn rollback_floor_cannot_decrease_and_rejected_update_is_atomic() {
        let path = path();
        let store = initialized(&path);
        let old_bytes = fs::read(&path).unwrap();
        let old_ledger = store.load_at(NOW).unwrap().audit_ledger;
        let mut replacement = fixture();
        replacement.rollback_floors[0].minimum_version = "2.1.0".into();
        assert!(matches!(
            store.replace_at(1, replacement, NOW + 1),
            Err(ExtensionPolicyError::RollbackFloor)
        ));
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        let after_rejection = store.load_at(NOW).unwrap();
        assert_eq!(after_rejection.revision, 1);
        assert_eq!(
            after_rejection.audit_ledger.events.len(),
            old_ledger.events.len()
        );
        assert_eq!(
            after_rejection.audit_ledger.next_sequence,
            old_ledger.next_sequence
        );
        remove_files(&path);
    }

    #[test]
    fn replacements_advance_revision_and_conflicting_updates_do_not_overwrite() {
        let path = path();
        let store = initialized(&path);
        let mut replacement = fixture();
        replacement.issued_at_unix_seconds = NOW + 1;
        replacement.valid_until_unix_seconds = NOW + 31 * 24 * 60 * 60;
        assert_eq!(
            store
                .replace_at(1, replacement.clone(), NOW + 1)
                .unwrap()
                .revision,
            2
        );
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            store.replace_at(1, replacement, NOW + 2),
            Err(ExtensionPolicyError::RevisionConflict { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        remove_files(&path);
    }
}
