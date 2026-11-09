//! Verifies signed release metadata before any artifact can be staged.
//!
//! The detached signature authenticates the exact manifest bytes. BLAKE3
//! identities in that manifest then describe the individual binary and tool
//! files. Verification and installation are separate operations.

use std::collections::BTreeSet;

use ed25519_dalek::{Signature, VerifyingKey};
use jumpstart_domain::{BlobId, Counter};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One protocol version for the minimum and maximum supported wire contract.
#[derive(Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

/// Inclusive versions a release can read or write without converting storage.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageRange {
    pub min_format: u32,
    pub max_format: u32,
    pub min_schema: u32,
    pub max_schema: u32,
}

impl StorageRange {
    /// Return whether `format` and `schema` both lie within this range.
    pub fn contains(&self, format: u32, schema: u32) -> bool {
        (self.min_format..=self.max_format).contains(&format)
            && (self.min_schema..=self.max_schema).contains(&schema)
    }
}

/// Exact bytes to verify at one release-relative path before staging.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub path: String,
    pub bytes: Counter,
    pub hash: BlobId,
}

/// Describes a candidate release. Use `verify_release_manifest` to authenticate it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub manifest_version: u16,
    pub release_id: String,
    pub protocol_min: ProtocolVersion,
    pub protocol_max: ProtocolVersion,
    pub readable_storage: StorageRange,
    pub writable_storage: StorageRange,
    pub migration_ids: Vec<String>,
    pub recovery_tool_version: String,
    pub artifacts: Vec<ReleaseArtifact>,
}

/// Distinguishes a trust failure from a signed but invalid release contract.
#[derive(Debug, Error)]
pub enum ReleaseManifestError {
    #[error("release manifest is empty or exceeds its byte limit")]
    Size,
    #[error("release manifest signature did not verify against the pinned key")]
    Signature,
    #[error("release manifest JSON failed validation: {0}")]
    Decode(#[source] crate::CodecError),
    #[error("invalid release manifest: {0}")]
    Invalid(&'static str),
}

/// Verify the detached signature and return the checked manifest.
///
/// `bytes` are the exact signed JSON bytes, `signature` is the detached Ed25519
/// signature, and `public_key` comes from a trusted location outside the bundle.
/// Rejects malformed JSON, unsafe or duplicate paths, inverted compatibility
/// ranges, and a package with no server binary. The caller verifies artifacts.
pub fn verify_release_manifest(
    bytes: &[u8],
    signature: &[u8; 64],
    public_key: &[u8; 32],
) -> Result<ReleaseManifest, ReleaseManifestError> {
    if bytes.is_empty() || bytes.len() > crate::MAX_FRAME_BYTES {
        return Err(ReleaseManifestError::Size);
    }
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| ReleaseManifestError::Signature)?;
    key.verify_strict(bytes, &Signature::from_bytes(signature))
        .map_err(|_| ReleaseManifestError::Signature)?;
    let manifest: ReleaseManifest =
        crate::decode_body(bytes).map_err(ReleaseManifestError::Decode)?;
    validate(&manifest)?;
    Ok(manifest)
}

/// Validate the compatibility and path invariants of an authenticated manifest.
fn validate(manifest: &ReleaseManifest) -> Result<(), ReleaseManifestError> {
    if manifest.manifest_version != 1 {
        return Err(ReleaseManifestError::Invalid(
            "unsupported manifest version",
        ));
    }
    if !safe_component(&manifest.release_id)
        || !manifest
            .release_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ReleaseManifestError::Invalid("unsafe release identifier"));
    }
    if manifest.protocol_min.major == 0 || manifest.protocol_min > manifest.protocol_max {
        return Err(ReleaseManifestError::Invalid("invalid protocol range"));
    }
    let read = &manifest.readable_storage;
    let write = &manifest.writable_storage;
    if read.min_format == 0
        || read.min_schema == 0
        || write.min_format > write.max_format
        || write.min_schema > write.max_schema
        || !read.contains(write.min_format, write.min_schema)
        || !read.contains(write.max_format, write.max_schema)
    {
        return Err(ReleaseManifestError::Invalid(
            "invalid storage compatibility ranges",
        ));
    }
    let mut migrations = BTreeSet::new();
    if manifest.recovery_tool_version.trim().is_empty()
        || manifest
            .migration_ids
            .iter()
            .any(|id| id.trim().is_empty() || !migrations.insert(id.as_str()))
    {
        return Err(ReleaseManifestError::Invalid(
            "invalid recovery or migration identity",
        ));
    }
    let mut paths = BTreeSet::new();
    for artifact in &manifest.artifacts {
        if !artifact.path.split('/').all(safe_component)
            || matches!(artifact.path.as_str(), "release.json" | "release.sig")
            || !paths.insert(artifact.path.as_str())
        {
            return Err(ReleaseManifestError::Invalid(
                "unsafe or duplicate artifact path",
            ));
        }
    }
    if !paths.contains("bin/jumpstart-server") {
        return Err(ReleaseManifestError::Invalid("server binary is absent"));
    }
    Ok(())
}

/// Return whether `value` can be one ordinary release path component.
fn safe_component(value: &str) -> bool {
    !value.is_empty() && !matches!(value, "." | "..") && !value.contains(['\0', '/', '\\'])
}
