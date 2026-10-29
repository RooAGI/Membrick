//! Host-owned identity and integrity checks for future downloadable extensions.
//!
//! This verifier is deliberately not connected to the current link-time
//! extension registry. Static registrations are native code already linked
//! into the host executable and require a trusted host build/release process.

use ed25519_dalek::{Signature, VerifyingKey};
use membrick_extension_api::{OwnedExtensionDescriptor, OwnedToolDescriptor, OwnedToolId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

const MANIFEST_DOMAIN: &[u8] = b"Membrick-Extension-Manifest-v1\0";
const MANIFEST_VERSION: u32 = 1;
const HOST_API_ID: &str = "membrick.extension-api";
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_ARTIFACT_BYTES: usize = 128 * 1024 * 1024;
const MAX_MANIFEST_LIST_ITEMS: usize = 128;
const ED25519_PUBLIC_KEY_HEX_LEN: usize = 64;
const ED25519_SIGNATURE_HEX_LEN: usize = 128;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub manifest_version: u32,
    pub extension_id: String,
    pub version: String,
    pub host_api: HostApiRequirement,
    pub artifact: ExtensionArtifact,
    pub publisher: ExtensionPublisher,
    pub source: Option<SourceProvenance>,
    pub provides: Vec<String>,
    pub requires: Vec<CapabilityRequirement>,
    /// Generic host-facing tool metadata covered by the manifest signature.
    /// These declarations do not include or activate executable handlers.
    #[serde(default)]
    pub tools: Vec<OwnedToolDescriptor>,
    pub requested_permissions: Vec<PermissionRequest>,
}

impl ExtensionManifest {
    /// Convert signed declarations to the owned metadata-only registry form.
    /// Capability version requirements remain signed claims and are checked by
    /// admission; the metadata registry validates dependency identities only.
    pub fn owned_descriptor(&self) -> Result<OwnedExtensionDescriptor, TrustError> {
        OwnedExtensionDescriptor::new(
            self.extension_id.clone(),
            self.provides.clone(),
            self.requires
                .iter()
                .map(|requirement| requirement.capability.clone())
                .collect(),
            self.tools.clone(),
        )
        .map_err(|error| TrustError::Manifest(error.to_string()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostApiRequirement {
    pub id: String,
    pub version_requirement: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionArtifact {
    pub format: String,
    pub target: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPublisher {
    pub id: String,
    pub key_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProvenance {
    pub repository: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirement {
    pub capability: String,
    pub version_requirement: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequest {
    pub permission: String,
    pub scope: String,
}

/// A host-configured publisher key and the extension IDs it may sign.
///
/// `active` is host policy state, never read from the extension manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPublisherKey {
    pub publisher_id: String,
    pub key_id: String,
    /// 32-byte Ed25519 public key encoded as 64 hexadecimal characters.
    pub public_key_hex: String,
    pub allowed_extension_ids: Vec<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustPolicy {
    pub host_api_version: String,
    pub publisher_keys: Vec<TrustedPublisherKey>,
}

/// Evidence that a manifest signature and artifact digest were verified.
///
/// This is an integrity and publisher-identity result only. It does not admit,
/// authorize, grant permissions to, or activate an extension. Callers must
/// separately inspect `manifest` and apply host admission and permission policy.
#[derive(Debug)]
pub struct VerifiedArtifact {
    /// The complete typed claims covered by the verified manifest signature.
    manifest: ExtensionManifest,
    /// SHA-256 of the canonical JCS manifest whose signature was verified.
    manifest_sha256: String,
    /// SHA-256 computed from the exact artifact bytes supplied to verification.
    artifact_sha256: String,
}

impl VerifiedArtifact {
    /// Read the claims covered by the verified signature.
    pub fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    /// Read the digest computed from the artifact bytes supplied to verification.
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    /// Read the digest of the canonical manifest covered by the signature.
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustError {
    Manifest(String),
    Policy(String),
    Signature,
    ArtifactDigest,
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(message) => write!(f, "invalid extension manifest: {message}"),
            Self::Policy(message) => {
                write!(f, "extension rejected by host trust policy: {message}")
            }
            Self::Signature => f.write_str("extension manifest signature is invalid"),
            Self::ArtifactDigest => {
                f.write_str("extension artifact digest does not match manifest")
            }
        }
    }
}

impl std::error::Error for TrustError {}

/// Verify a signed manifest and the exact artifact bytes supplied by the caller.
/// The caller must retain and later use those same immutable bytes.
///
/// The signature is Ed25519 over
/// `MANIFEST_DOMAIN || RFC8785_JCS(manifest)`. The signer is resolved from the
/// host-owned `TrustPolicy`; publisher/key claims in the manifest do not
/// establish trust on their own. Success proves signature and digest integrity
/// under that policy; it is not extension admission or authorization.
/// Inputs are bounded to a 256 KiB manifest, a 128 MiB artifact, and 128 items
/// in each manifest capability, tool, and permission list.
pub fn verify_extension_artifact(
    manifest_json: &[u8],
    signature_hex: &str,
    artifact_bytes: &[u8],
    policy: &TrustPolicy,
) -> Result<VerifiedArtifact, TrustError> {
    check_manifest_size(manifest_json.len())?;
    check_artifact_size(artifact_bytes.len())?;
    if signature_hex.len() != ED25519_SIGNATURE_HEX_LEN {
        return Err(TrustError::Manifest(
            "Ed25519 signature must be 64 bytes encoded as 128 hex characters".into(),
        ));
    }

    let manifest: ExtensionManifest = serde_json::from_slice(manifest_json)
        .map_err(|error| TrustError::Manifest(error.to_string()))?;
    // Canonicalize the supplied JSON value itself. Re-serializing the typed
    // manifest can add an absent optional field (for example `source: null`),
    // which would verify a different signed payload. The typed parse above
    // rejects duplicate keys and unknown fields before this generic parse.
    let manifest_value: serde_json::Value = serde_json::from_slice(manifest_json)
        .map_err(|error| TrustError::Manifest(error.to_string()))?;
    if manifest.provides.len() > MAX_MANIFEST_LIST_ITEMS
        || manifest.requires.len() > MAX_MANIFEST_LIST_ITEMS
        || manifest.tools.len() > MAX_MANIFEST_LIST_ITEMS
        || manifest.requested_permissions.len() > MAX_MANIFEST_LIST_ITEMS
    {
        return Err(TrustError::Manifest(format!(
            "manifest capability, tool, and permission lists may contain at most {MAX_MANIFEST_LIST_ITEMS} items"
        )));
    }

    // Deserialize preserves the owned declaration payload, while this build
    // validates IDs, duplicate tool IDs, schema object shape, and bounded
    // descriptions/schema sizes before accepting signed claims.
    validate_tool_declarations(&manifest)?;
    manifest.owned_descriptor()?;

    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(TrustError::Manifest("unsupported manifest_version".into()));
    }
    semver::Version::parse(&manifest.version)
        .map_err(|_| TrustError::Manifest("version must be valid SemVer".into()))?;
    if manifest.extension_id.trim().is_empty()
        || manifest.publisher.id.trim().is_empty()
        || manifest.publisher.key_id.trim().is_empty()
    {
        return Err(TrustError::Manifest(
            "identity fields must be non-empty".into(),
        ));
    }
    if manifest.artifact.format != "wasm-core-module" {
        return Err(TrustError::Manifest("unsupported artifact format".into()));
    }
    if manifest.artifact.target != "wasm32-unknown-unknown" {
        return Err(TrustError::Manifest(
            "unsupported Wasm core-module target".into(),
        ));
    }
    if manifest.host_api.id != HOST_API_ID {
        return Err(TrustError::Manifest("unsupported host API".into()));
    }
    let api_requirement = semver::VersionReq::parse(&manifest.host_api.version_requirement)
        .map_err(|_| TrustError::Manifest("invalid host_api version_requirement".into()))?;
    let host_api_version = semver::Version::parse(&policy.host_api_version)
        .map_err(|_| TrustError::Policy("host API version is invalid".into()))?;
    if !api_requirement.matches(&host_api_version) {
        return Err(TrustError::Policy(
            "host API version is incompatible".into(),
        ));
    }

    let mut matching_keys = policy.publisher_keys.iter().filter(|key| {
        key.publisher_id == manifest.publisher.id && key.key_id == manifest.publisher.key_id
    });
    let trusted_key = matching_keys
        .next()
        .ok_or_else(|| TrustError::Policy("publisher key is not trusted".into()))?;
    if matching_keys.next().is_some() {
        return Err(TrustError::Policy(
            "publisher key configuration is ambiguous".into(),
        ));
    }
    if !trusted_key.active {
        return Err(TrustError::Policy(
            "publisher key is revoked or inactive".into(),
        ));
    }
    if !trusted_key
        .allowed_extension_ids
        .iter()
        .any(|id| id == &manifest.extension_id)
    {
        return Err(TrustError::Policy(
            "publisher is not authorized for this extension ID".into(),
        ));
    }

    if trusted_key.public_key_hex.len() != ED25519_PUBLIC_KEY_HEX_LEN {
        return Err(TrustError::Policy(
            "Ed25519 public key must be 32 bytes encoded as 64 hex characters".into(),
        ));
    }
    let public_key_bytes: [u8; 32] = decode_hex(&trusted_key.public_key_hex)?
        .try_into()
        .map_err(|_| TrustError::Policy("Ed25519 public key must be 32 bytes".into()))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|_| TrustError::Policy("configured Ed25519 public key is invalid".into()))?;
    let signature_bytes: [u8; 64] = decode_hex(signature_hex)?
        .try_into()
        .map_err(|_| TrustError::Manifest("Ed25519 signature must be 64 bytes".into()))?;
    let signature = Signature::from_bytes(&signature_bytes);

    let canonical_manifest = serde_jcs::to_vec(&manifest_value)
        .map_err(|error| TrustError::Manifest(error.to_string()))?;
    let manifest_sha256 = format!("sha256:{:x}", Sha256::digest(&canonical_manifest));
    let mut signed_payload = Vec::with_capacity(MANIFEST_DOMAIN.len() + canonical_manifest.len());
    signed_payload.extend_from_slice(MANIFEST_DOMAIN);
    signed_payload.extend_from_slice(&canonical_manifest);
    verifying_key
        .verify_strict(&signed_payload, &signature)
        .map_err(|_| TrustError::Signature)?;

    let computed_digest = format!("sha256:{:x}", Sha256::digest(artifact_bytes));
    if manifest.artifact.sha256 != computed_digest {
        return Err(TrustError::ArtifactDigest);
    }

    Ok(VerifiedArtifact {
        manifest,
        manifest_sha256,
        artifact_sha256: computed_digest,
    })
}

fn validate_tool_declarations(manifest: &ExtensionManifest) -> Result<(), TrustError> {
    let mut ids = std::collections::BTreeSet::new();
    for tool in &manifest.tools {
        OwnedToolId::try_new(tool.id.as_str())
            .map_err(|error| TrustError::Manifest(error.to_string()))?;
        if !ids.insert(tool.id.as_str()) {
            return Err(TrustError::Manifest(format!(
                "duplicate tool ID `{}`",
                tool.id.as_str()
            )));
        }
        if tool.description.trim().is_empty() || tool.description.len() > 4096 {
            return Err(TrustError::Manifest(format!(
                "tool `{}` has an invalid description",
                tool.id.as_str()
            )));
        }
        if !tool.input_schema.is_object() {
            return Err(TrustError::Manifest(format!(
                "tool `{}` input_schema must be a JSON object",
                tool.id.as_str()
            )));
        }
        let schema_bytes = serde_json::to_vec(&tool.input_schema)
            .map_err(|error| TrustError::Manifest(error.to_string()))?;
        if schema_bytes.len() > 64 * 1024 {
            return Err(TrustError::Manifest(format!(
                "tool `{}` input_schema exceeds 65536 bytes",
                tool.id.as_str()
            )));
        }
    }
    Ok(())
}

fn check_manifest_size(size: usize) -> Result<(), TrustError> {
    if size > MAX_MANIFEST_BYTES {
        return Err(TrustError::Manifest(format!(
            "manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn check_artifact_size(size: usize) -> Result<(), TrustError> {
    if size > MAX_ARTIFACT_BYTES {
        return Err(TrustError::Manifest(format!(
            "artifact exceeds the {MAX_ARTIFACT_BYTES}-byte verification limit"
        )));
    }
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, TrustError> {
    if value.len() % 2 != 0 {
        return Err(TrustError::Manifest("hex value has odd length".into()));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])
                .ok_or_else(|| TrustError::Manifest("invalid hex value".into()))?;
            let low = hex_nibble(pair[1])
                .ok_or_else(|| TrustError::Manifest("invalid hex value".into()))?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const ARTIFACT: &[u8] = b"test wasm component bytes";

    fn manifest_json() -> String {
        format!(
            r#"{{
              "manifest_version":1,
              "extension_id":"org.example.flowboard",
              "version":"1.4.2",
              "host_api":{{"id":"membrick.extension-api","version_requirement":">=0.1.0, <0.2.0"}},
              "artifact":{{"format":"wasm-core-module","target":"wasm32-unknown-unknown","sha256":"sha256:{:x}"}},
              "publisher":{{"id":"org.example","key_id":"release-1"}},
              "source":{{"repository":"https://example.test/flowboard","revision":"abc123"}},
              "provides":["board-post.v1"],
              "requires":[],
              "requested_permissions":[{{"permission":"documents.read_write","scope":"owner_workspace"}}]
            }}"#,
            Sha256::digest(ARTIFACT)
        )
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn policy(signing_key: &SigningKey) -> TrustPolicy {
        TrustPolicy {
            host_api_version: "0.1.4".into(),
            publisher_keys: vec![TrustedPublisherKey {
                publisher_id: "org.example".into(),
                key_id: "release-1".into(),
                public_key_hex: encode_hex(signing_key.verifying_key().as_bytes()),
                allowed_extension_ids: vec!["org.example.flowboard".into()],
                active: true,
            }],
        }
    }

    fn signature_for(json: &str, signing_key: &SigningKey) -> String {
        let manifest: serde_json::Value = serde_json::from_str(json).unwrap();
        let canonical = serde_jcs::to_vec(&manifest).unwrap();
        let mut payload = MANIFEST_DOMAIN.to_vec();
        payload.extend(canonical);
        encode_hex(&signing_key.sign(&payload).to_bytes())
    }

    fn encode_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn verifies_trusted_manifest_and_artifact() {
        let signing_key = key();
        let json = manifest_json();
        let signature = signature_for(&json, &signing_key);
        let verified = verify_extension_artifact(
            &json.into_bytes(),
            &signature,
            ARTIFACT,
            &policy(&signing_key),
        )
        .unwrap();
        assert_eq!(verified.manifest.extension_id, "org.example.flowboard");
        assert_eq!(verified.manifest.version, "1.4.2");
        assert_eq!(verified.manifest.artifact.target, "wasm32-unknown-unknown");
        assert_eq!(verified.manifest.provides, ["board-post.v1"]);
        assert_eq!(verified.manifest.requested_permissions.len(), 1);
        assert_eq!(verified.artifact_sha256, verified.manifest.artifact.sha256);
    }

    #[test]
    fn rejects_oversized_inputs_and_manifest_claim_lists() {
        assert!(matches!(
            check_manifest_size(MAX_MANIFEST_BYTES + 1),
            Err(TrustError::Manifest(_))
        ));
        assert!(matches!(
            check_artifact_size(MAX_ARTIFACT_BYTES + 1),
            Err(TrustError::Manifest(_))
        ));

        let signing_key = key();
        let json = manifest_json().replace(
            r#""provides":["board-post.v1"]"#,
            &format!(
                "\"provides\":[{}]",
                std::iter::repeat_n("\"capability\"", MAX_MANIFEST_LIST_ITEMS + 1)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        );
        let signature = signature_for(&json, &signing_key);
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &policy(&signing_key)),
            Err(TrustError::Manifest(_))
        ));
    }

    #[test]
    fn verifies_manifest_with_omitted_optional_source() {
        let signing_key = key();
        let json = manifest_json().replace(
            r#""source":{"repository":"https://example.test/flowboard","revision":"abc123"},"#,
            "",
        );
        let signature = signature_for(&json, &signing_key);
        assert!(verify_extension_artifact(
            json.as_bytes(),
            &signature,
            ARTIFACT,
            &policy(&signing_key)
        )
        .is_ok());
    }

    #[test]
    fn verifies_signed_generic_tool_declarations_and_converts_metadata() {
        let signing_key = key();
        let json = manifest_json().replace(
            r#""requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
            r#""tools":[{"id":"flowboard.status","description":"Read status","input_schema":{"type":"object","properties":{"board_id":{"type":"string"}}}}],"requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
        );
        let signature = signature_for(&json, &signing_key);
        let verified =
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &policy(&signing_key))
                .unwrap();

        assert_eq!(verified.manifest().tools.len(), 1);
        let descriptor = verified.manifest().owned_descriptor().unwrap();
        assert_eq!(descriptor.id().as_str(), "org.example.flowboard");
        assert_eq!(descriptor.tools()[0].id.as_str(), "flowboard.status");
        assert_eq!(descriptor.tools()[0].input_schema["type"], "object");
    }

    #[test]
    fn rejects_invalid_or_duplicate_signed_tool_declarations() {
        let signing_key = key();
        let duplicate_json = manifest_json().replace(
            r#""requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
            r#""tools":[{"id":"example.status","description":"Read status","input_schema":{"type":"object"}},{"id":"example.status","description":"Duplicate","input_schema":{"type":"object"}}],"requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
        );
        let duplicate_signature = signature_for(&duplicate_json, &signing_key);
        assert!(matches!(
            verify_extension_artifact(
                duplicate_json.as_bytes(),
                &duplicate_signature,
                ARTIFACT,
                &policy(&signing_key)
            ),
            Err(TrustError::Manifest(_))
        ));

        let invalid_json = manifest_json().replace(
            r#""requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
            r#""tools":[{"id":"tool with spaces","description":"Bad ID","input_schema":{"type":"object"}}],"requested_permissions":[{"permission":"documents.read_write","scope":"owner_workspace"}]"#,
        );
        let invalid_signature = signature_for(&invalid_json, &signing_key);
        assert!(matches!(
            verify_extension_artifact(
                invalid_json.as_bytes(),
                &invalid_signature,
                ARTIFACT,
                &policy(&signing_key)
            ),
            Err(TrustError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_malformed_manifest_json() {
        let signing_key = key();
        let json = b"{\"manifest_version\":1";
        assert!(matches!(
            verify_extension_artifact(json, "00", ARTIFACT, &policy(&signing_key)),
            Err(TrustError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_duplicate_manifest_fields() {
        let json = manifest_json().replace(
            "\"manifest_version\":1,",
            "\"manifest_version\":1,\"manifest_version\":1,",
        );
        let signing_key = key();
        let signature = signature_for(&manifest_json(), &signing_key);
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &policy(&signing_key)),
            Err(TrustError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_untrusted_or_revoked_keys_and_unauthorized_ids() {
        let signing_key = key();
        let json = manifest_json();
        let signature = signature_for(&json, &signing_key);
        let mut p = policy(&signing_key);
        p.publisher_keys[0].active = false;
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &p),
            Err(TrustError::Policy(_))
        ));
        let mut p = policy(&signing_key);
        p.publisher_keys[0].allowed_extension_ids.clear();
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &p),
            Err(TrustError::Policy(_))
        ));
        let p = TrustPolicy {
            publisher_keys: vec![],
            ..policy(&signing_key)
        };
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &p),
            Err(TrustError::Policy(_))
        ));
    }

    #[test]
    fn rejects_invalid_signature_artifact_digest_and_host_api_range() {
        let signing_key = key();
        let json = manifest_json();
        let signature = signature_for(&json, &signing_key);
        let mut wrong_sig = signature.clone().into_bytes();
        wrong_sig[0] = if wrong_sig[0] == b'0' { b'1' } else { b'0' };
        assert!(matches!(
            verify_extension_artifact(
                json.as_bytes(),
                std::str::from_utf8(&wrong_sig).unwrap(),
                ARTIFACT,
                &policy(&signing_key)
            ),
            Err(TrustError::Signature)
        ));
        assert!(matches!(
            verify_extension_artifact(
                json.as_bytes(),
                &signature,
                b"different artifact",
                &policy(&signing_key)
            ),
            Err(TrustError::ArtifactDigest)
        ));
        let mut p = policy(&signing_key);
        p.host_api_version = "0.2.0".into();
        assert!(matches!(
            verify_extension_artifact(json.as_bytes(), &signature, ARTIFACT, &p),
            Err(TrustError::Policy(_))
        ));
    }
}
