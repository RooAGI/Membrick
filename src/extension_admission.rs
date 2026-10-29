//! Host-owned admission policy for future downloadable extensions.
//!
//! This is deliberately separate from the compiled-in `linkme` registry.
//! Passing admission produces an immutable byte-bearing value for a future
//! loader; it does not load code or activate an extension.

use crate::extension_trust::{ExtensionManifest, VerifiedArtifact};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

const HOST_API_ID: &str = "membrick.extension-api";
const MAX_ADMISSION_ARTIFACT_BYTES: usize = 128 * 1024 * 1024;

/// One capability available to a newly admitted extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableCapability {
    pub id: String,
    pub version: String,
}

/// A host-granted permission, bound to one exact permission name and scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionGrant {
    pub permission: String,
    pub scope: String,
}

/// The exact release tuple approved by the host operator.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedExtension {
    pub extension_id: String,
    pub version: String,
    pub minimum_version: String,
    /// Digest of the canonical JCS manifest approved for this release.
    pub manifest_sha256: String,
    pub publisher_id: String,
    pub publisher_key_id: String,
    pub artifact_sha256: String,
    pub target: String,
    /// Capabilities this exact approved release is allowed to provide.
    pub provides: Vec<String>,
    /// Permissions granted to this exact approved release.
    pub grants: Vec<PermissionGrant>,
}

/// Host-owned inputs for deciding whether a verified artifact may be admitted.
///
/// The policy is supplied by the host, independently from the extension
/// package. `active_extension_ids` represents identities already admitted by
/// the caller's current runtime inventory; it prevents a second artifact from
/// claiming the same ID in one runtime.
#[derive(Clone, Debug)]
pub struct AdmissionPolicy {
    pub host_api_version: String,
    pub approved_extensions: Vec<ApprovedExtension>,
    pub available_capabilities: Vec<AvailableCapability>,
    pub active_extension_ids: Vec<String>,
}

/// An artifact whose signed identity, exact bytes, compatibility, and grants
/// were checked against one host-owned admission policy.
///
/// Fields are private and the type is not `Clone`. A future loader should read
/// the executable bytes through [`Self::artifact_bytes`] rather than reopening
/// a package path. This type is not wired to the current static registry.
pub struct AdmittedArtifact {
    verified: VerifiedArtifact,
    artifact_bytes: Arc<[u8]>,
    effective_grants: Vec<PermissionGrant>,
}

impl fmt::Debug for AdmittedArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedArtifact")
            .field("extension_id", &self.manifest().extension_id)
            .field("version", &self.manifest().version)
            .field("artifact_sha256", &self.artifact_sha256())
            .field("artifact_bytes_len", &self.artifact_bytes.len())
            .field("effective_grants", &self.effective_grants)
            .finish()
    }
}

impl AdmittedArtifact {
    pub fn manifest(&self) -> &ExtensionManifest {
        self.verified.manifest()
    }

    pub fn artifact_sha256(&self) -> &str {
        self.verified.artifact_sha256()
    }

    /// The exact immutable bytes re-hashed during admission.
    pub fn artifact_bytes(&self) -> &[u8] {
        &self.artifact_bytes
    }

    /// The exact host-approved permission grants bound to this release.
    pub fn effective_grants(&self) -> &[PermissionGrant] {
        &self.effective_grants
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Policy(String),
    Rejected(String),
    ArtifactDigest,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(message) => {
                write!(formatter, "invalid extension admission policy: {message}")
            }
            Self::Rejected(message) => write!(formatter, "extension not admitted: {message}"),
            Self::ArtifactDigest => formatter
                .write_str("artifact bytes differ from the bytes whose digest was verified"),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Apply host policy to an integrity-verified artifact and take ownership of
/// the exact bytes that will be made available to a future loader.
///
/// `VerifiedArtifact` alone is not an authorization token. This function
/// re-hashes the consumed byte buffer, checks it against both the verified
/// digest and signed manifest, then evaluates the host's pinned release,
/// target, API, capability, duplicate-identity, rollback, and permission
/// policies. It never loads or activates code.
pub fn admit_artifact(
    verified: VerifiedArtifact,
    artifact_bytes: Vec<u8>,
    policy: &AdmissionPolicy,
) -> Result<AdmittedArtifact, AdmissionError> {
    if artifact_bytes.len() > MAX_ADMISSION_ARTIFACT_BYTES {
        return Err(AdmissionError::Rejected(format!(
            "artifact exceeds the {MAX_ADMISSION_ARTIFACT_BYTES}-byte admission limit"
        )));
    }

    let manifest = verified.manifest();
    let computed_digest = format!("sha256:{:x}", Sha256::digest(&artifact_bytes));
    if computed_digest != verified.artifact_sha256() || computed_digest != manifest.artifact.sha256
    {
        return Err(AdmissionError::ArtifactDigest);
    }

    let approved = approved_release(policy, manifest)?;
    validate_api_compatibility(policy, manifest)?;
    validate_capability_dependencies(policy, manifest)?;
    validate_release_claims(approved, manifest, verified.manifest_sha256())?;
    let effective_grants = validate_permissions(approved, manifest)?;

    Ok(AdmittedArtifact {
        verified,
        artifact_bytes: Arc::from(artifact_bytes),
        effective_grants,
    })
}

fn approved_release<'a>(
    policy: &'a AdmissionPolicy,
    manifest: &ExtensionManifest,
) -> Result<&'a ApprovedExtension, AdmissionError> {
    let mut matches = policy
        .approved_extensions
        .iter()
        .filter(|item| item.extension_id == manifest.extension_id);
    let approved = matches.next().ok_or_else(|| {
        AdmissionError::Rejected("extension identity is not approved by the host".into())
    })?;
    if matches.next().is_some() {
        return Err(AdmissionError::Policy(format!(
            "extension ID `{}` has duplicate approval entries",
            manifest.extension_id
        )));
    }
    if policy
        .active_extension_ids
        .iter()
        .filter(|id| *id == &manifest.extension_id)
        .count()
        > 0
    {
        return Err(AdmissionError::Rejected(format!(
            "extension ID `{}` is already active",
            manifest.extension_id
        )));
    }
    if policy
        .active_extension_ids
        .iter()
        .collect::<BTreeSet<_>>()
        .len()
        != policy.active_extension_ids.len()
    {
        return Err(AdmissionError::Policy(
            "active extension IDs contain duplicates".into(),
        ));
    }
    Ok(approved)
}

fn validate_api_compatibility(
    policy: &AdmissionPolicy,
    manifest: &ExtensionManifest,
) -> Result<(), AdmissionError> {
    if manifest.host_api.id != HOST_API_ID {
        return Err(AdmissionError::Rejected(
            "manifest targets an unsupported host API".into(),
        ));
    }
    let requirement = VersionReq::parse(&manifest.host_api.version_requirement)
        .map_err(|_| AdmissionError::Rejected("host API range is invalid".into()))?;
    let host_version = Version::parse(&policy.host_api_version)
        .map_err(|_| AdmissionError::Policy("host API version is invalid".into()))?;
    if !requirement.matches(&host_version) {
        return Err(AdmissionError::Rejected(
            "host API version is incompatible".into(),
        ));
    }
    Ok(())
}

fn validate_capability_dependencies(
    policy: &AdmissionPolicy,
    manifest: &ExtensionManifest,
) -> Result<(), AdmissionError> {
    let mut available = BTreeMap::<String, Version>::new();
    for capability in &policy.available_capabilities {
        if capability.id.trim().is_empty() {
            return Err(AdmissionError::Policy(
                "available capability IDs must not be empty".into(),
            ));
        }
        if available.contains_key(&capability.id) {
            return Err(AdmissionError::Policy(format!(
                "capability `{}` has duplicate available versions",
                capability.id
            )));
        }
        let version = Version::parse(&capability.version).map_err(|_| {
            AdmissionError::Policy(format!(
                "available capability `{}` has an invalid version",
                capability.id
            ))
        })?;
        available.insert(capability.id.clone(), version);
    }

    let mut seen = BTreeSet::new();
    for required in &manifest.requires {
        if !seen.insert(required.capability.as_str()) {
            return Err(AdmissionError::Rejected(format!(
                "manifest repeats dependency `{}`",
                required.capability
            )));
        }
        let available_version = available.get(&required.capability).ok_or_else(|| {
            AdmissionError::Rejected(format!(
                "required capability `{}` is unavailable",
                required.capability
            ))
        })?;
        let requirement = VersionReq::parse(&required.version_requirement).map_err(|_| {
            AdmissionError::Rejected(format!(
                "dependency range for `{}` is invalid",
                required.capability
            ))
        })?;
        if !requirement.matches(available_version) {
            return Err(AdmissionError::Rejected(format!(
                "required capability `{}` has an incompatible version",
                required.capability
            )));
        }
    }
    Ok(())
}

fn validate_release_claims(
    approved: &ApprovedExtension,
    manifest: &ExtensionManifest,
    manifest_sha256: &str,
) -> Result<(), AdmissionError> {
    if manifest.artifact.format != "wasm-core-module"
        || manifest.artifact.target != "wasm32-unknown-unknown"
    {
        return Err(AdmissionError::Rejected(
            "unsupported artifact format".into(),
        ));
    }
    if manifest.artifact.target != approved.target {
        return Err(AdmissionError::Rejected(
            "artifact target is not approved by the host".into(),
        ));
    }
    if manifest.artifact.sha256 != approved.artifact_sha256 {
        return Err(AdmissionError::Rejected(
            "artifact digest is not pinned by host policy".into(),
        ));
    }
    if manifest_sha256 != approved.manifest_sha256 {
        return Err(AdmissionError::Rejected(
            "canonical manifest digest is not pinned by host policy".into(),
        ));
    }
    if manifest.publisher.id != approved.publisher_id
        || manifest.publisher.key_id != approved.publisher_key_id
    {
        return Err(AdmissionError::Rejected(
            "publisher identity or signing key does not match the approved release".into(),
        ));
    }
    if manifest.version != approved.version {
        return Err(AdmissionError::Rejected(
            "extension version does not match the approved release".into(),
        ));
    }
    let version = Version::parse(&manifest.version)
        .map_err(|_| AdmissionError::Rejected("extension version is invalid".into()))?;
    let minimum = Version::parse(&approved.minimum_version)
        .map_err(|_| AdmissionError::Policy("minimum extension version is invalid".into()))?;
    if version < minimum {
        return Err(AdmissionError::Rejected(
            "release is older than the host's rollback floor".into(),
        ));
    }

    let actual = unique_strings(&manifest.provides, "provided capability")?;
    let expected = unique_strings(&approved.provides, "approved capability")?;
    if actual != expected {
        return Err(AdmissionError::Rejected(
            "provided capabilities do not match the approved release".into(),
        ));
    }
    if actual.iter().any(|provided| {
        manifest
            .requires
            .iter()
            .any(|required| required.capability == *provided)
    }) {
        return Err(AdmissionError::Rejected(
            "extension cannot require a capability that it provides".into(),
        ));
    }
    Ok(())
}

fn validate_permissions(
    approved: &ApprovedExtension,
    manifest: &ExtensionManifest,
) -> Result<Vec<PermissionGrant>, AdmissionError> {
    let granted = approved.grants.iter().cloned().collect::<BTreeSet<_>>();
    if granted.len() != approved.grants.len() {
        return Err(AdmissionError::Policy(
            "approved release contains duplicate permission grants".into(),
        ));
    }
    if granted
        .iter()
        .any(|grant| grant.permission.trim().is_empty() || grant.scope.trim().is_empty())
    {
        return Err(AdmissionError::Policy(
            "permission grants must name a permission and scope".into(),
        ));
    }
    let mut requested = BTreeSet::new();
    let mut effective_grants = Vec::new();
    for permission in &manifest.requested_permissions {
        if permission.permission.trim().is_empty() || permission.scope.trim().is_empty() {
            return Err(AdmissionError::Rejected(
                "permission requests must name a permission and scope".into(),
            ));
        }
        let request = PermissionGrant {
            permission: permission.permission.clone(),
            scope: permission.scope.clone(),
        };
        if !requested.insert(request.clone()) {
            return Err(AdmissionError::Rejected(format!(
                "manifest repeats permission `{}` at scope `{}`",
                request.permission, request.scope
            )));
        }
        if !granted.contains(&request) {
            return Err(AdmissionError::Rejected(format!(
                "permission `{}` at scope `{}` is not granted for this release",
                request.permission, request.scope
            )));
        }
        effective_grants.push(request);
    }
    Ok(effective_grants)
}

fn unique_strings(values: &[String], label: &str) -> Result<BTreeSet<String>, AdmissionError> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(AdmissionError::Rejected(format!(
            "{label} IDs must not be empty"
        )));
    }
    let set = values.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != values.len() {
        return Err(AdmissionError::Rejected(format!(
            "manifest repeats {label}"
        )));
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_trust::{verify_extension_artifact, TrustPolicy, TrustedPublisherKey};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    const ARTIFACT: &[u8] = b"exact immutable wasm component bytes";
    const MANIFEST_DOMAIN: &[u8] = b"Membrick-Extension-Manifest-v1\0";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11_u8; 32])
    }

    fn trusted(policy_key: &SigningKey) -> TrustPolicy {
        TrustPolicy {
            host_api_version: "0.1.4".into(),
            publisher_keys: vec![TrustedPublisherKey {
                publisher_id: "org.example".into(),
                key_id: "release-1".into(),
                public_key_hex: hex(policy_key.verifying_key().as_bytes()),
                allowed_extension_ids: vec!["org.example.flowboard".into()],
                active: true,
            }],
        }
    }

    fn manifest(artifact: &[u8]) -> serde_json::Value {
        json!({
            "manifest_version": 1,
            "extension_id": "org.example.flowboard",
            "version": "1.4.2",
            "host_api": {"id": HOST_API_ID, "version_requirement": ">=0.1.0, <0.2.0"},
            "artifact": {
                "format": "wasm-core-module",
                "target": "wasm32-unknown-unknown",
                "sha256": format!("sha256:{:x}", Sha256::digest(artifact))
            },
            "publisher": {"id": "org.example", "key_id": "release-1"},
            "provides": ["flowrun.v1"],
            "requires": [{"capability": "board-post.v1", "version_requirement": "^1.0"}],
            "requested_permissions": [{"permission": "documents.read_write", "scope": "owner_workspace"}]
        })
    }

    fn verified(artifact: &[u8]) -> VerifiedArtifact {
        verified_manifest(manifest(artifact), artifact)
    }

    fn verified_manifest(manifest: serde_json::Value, artifact: &[u8]) -> VerifiedArtifact {
        let key = signing_key();
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let canonical = serde_jcs::to_vec(&manifest).unwrap();
        let mut payload = MANIFEST_DOMAIN.to_vec();
        payload.extend(canonical);
        let signature = hex(&key.sign(&payload).to_bytes());
        verify_extension_artifact(&manifest_bytes, &signature, artifact, &trusted(&key)).unwrap()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn admission_policy(artifact: &[u8]) -> AdmissionPolicy {
        let manifest_value = manifest(artifact);
        let manifest_sha256 = format!(
            "sha256:{:x}",
            Sha256::digest(serde_jcs::to_vec(&manifest_value).unwrap())
        );
        AdmissionPolicy {
            host_api_version: "0.1.4".into(),
            approved_extensions: vec![ApprovedExtension {
                extension_id: "org.example.flowboard".into(),
                version: "1.4.2".into(),
                minimum_version: "1.4.0".into(),
                manifest_sha256,
                publisher_id: "org.example".into(),
                publisher_key_id: "release-1".into(),
                artifact_sha256: format!("sha256:{:x}", Sha256::digest(artifact)),
                target: "wasm32-unknown-unknown".into(),
                provides: vec!["flowrun.v1".into()],
                grants: vec![
                    PermissionGrant {
                        permission: "documents.read_write".into(),
                        scope: "owner_workspace".into(),
                    },
                    PermissionGrant {
                        permission: "documents.read".into(),
                        scope: "owner_workspace".into(),
                    },
                ],
            }],
            available_capabilities: vec![AvailableCapability {
                id: "board-post.v1".into(),
                version: "1.2.0".into(),
            }],
            active_extension_ids: Vec::new(),
        }
    }

    #[test]
    fn admits_only_the_pinned_release_and_owns_its_exact_bytes() {
        let verified = verified(ARTIFACT);
        let admitted =
            admit_artifact(verified, ARTIFACT.to_vec(), &admission_policy(ARTIFACT)).unwrap();
        assert_eq!(admitted.manifest().extension_id, "org.example.flowboard");
        assert_eq!(admitted.artifact_bytes(), ARTIFACT);
        assert_eq!(
            admitted.effective_grants(),
            &[PermissionGrant {
                permission: "documents.read_write".into(),
                scope: "owner_workspace".into(),
            }]
        );
        assert_eq!(
            admitted.artifact_sha256(),
            format!("sha256:{:x}", Sha256::digest(ARTIFACT))
        );
    }

    #[test]
    fn admitted_artifact_still_fails_closed_at_activation_boundary() {
        let admitted = admit_artifact(
            verified(ARTIFACT),
            ARTIFACT.to_vec(),
            &admission_policy(ARTIFACT),
        )
        .unwrap();

        let error = crate::extension_activation::activate_admitted_extension(admitted).unwrap_err();
        assert_eq!(
            error.blockers(),
            &[
                crate::extension_activation::ActivationBlocker::MaintainedSandboxProfileUnavailable,
                crate::extension_activation::ActivationBlocker::OwnedRegistryBridgeUnavailable,
                crate::extension_activation::ActivationBlocker::CapabilityRpcUnavailable,
                crate::extension_activation::ActivationBlocker::ScopedDocumentAuthorizationUnavailable,
            ]
        );
    }

    #[test]
    fn rejects_replaced_bytes_unapproved_target_and_ungranted_permissions() {
        assert_eq!(
            admit_artifact(
                verified(ARTIFACT),
                b"replacement bytes".to_vec(),
                &admission_policy(ARTIFACT)
            )
            .unwrap_err(),
            AdmissionError::ArtifactDigest
        );

        let mut policy = admission_policy(ARTIFACT);
        policy.approved_extensions[0].target = "wasm32-wasip1".into();
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));

        let mut policy = admission_policy(ARTIFACT);
        policy.approved_extensions[0].grants.clear();
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));
    }

    #[test]
    fn rejects_missing_dependency_duplicate_identity_and_rollback() {
        let mut policy = admission_policy(ARTIFACT);
        policy.available_capabilities.clear();
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));

        let mut policy = admission_policy(ARTIFACT);
        policy.approved_extensions.clear();
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));

        let mut policy = admission_policy(ARTIFACT);
        policy
            .approved_extensions
            .push(policy.approved_extensions[0].clone());
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Policy(_))
        ));

        let mut policy = admission_policy(ARTIFACT);
        policy
            .active_extension_ids
            .push("org.example.flowboard".into());
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));

        let mut policy = admission_policy(ARTIFACT);
        policy.approved_extensions[0].minimum_version = "1.5.0".into();
        assert!(matches!(
            admit_artifact(verified(ARTIFACT), ARTIFACT.to_vec(), &policy),
            Err(AdmissionError::Rejected(_))
        ));
    }

    #[test]
    fn rejects_re_signed_manifest_with_changed_claims_for_same_artifact() {
        let mut changed_manifest = manifest(ARTIFACT);
        changed_manifest["requires"] = json!([]);
        let verified = verified_manifest(changed_manifest, ARTIFACT);

        assert!(matches!(
            admit_artifact(verified, ARTIFACT.to_vec(), &admission_policy(ARTIFACT)),
            Err(AdmissionError::Rejected(_))
        ));
    }
}
