//! Checks signature trust and release-manifest invariants using fixture keys.

use ed25519_dalek::{Signer, SigningKey};
use jumpstart_domain::BlobId;
use jumpstart_protocol::release::{ReleaseManifestError, verify_release_manifest};
use serde_json::{Value, json};

/// Make one well-formed manifest value that tests can alter before signing.
fn manifest() -> Value {
    json!({
        "manifest_version": 1,
        "release_id": "0.1.0",
        "protocol_min": {"major": 1, "minor": 0},
        "protocol_max": {"major": 1, "minor": 1},
        "readable_storage": {"min_format": 1, "max_format": 1, "min_schema": 3, "max_schema": 5},
        "writable_storage": {"min_format": 1, "max_format": 1, "min_schema": 4, "max_schema": 5},
        "migration_ids": ["shared_application_catalog"],
        "recovery_tool_version": "0.1.0",
        "artifacts": [{"path": "bin/jumpstart-server", "bytes": "3", "hash": BlobId::hash_bytes(b"exe")}]
    })
}

/// The pinned signer and exact bytes determine whether metadata can be trusted.
#[test]
fn rejects_changed_bytes_and_a_different_signer() {
    let signer = SigningKey::from_bytes(&[7; 32]);
    let other = SigningKey::from_bytes(&[8; 32]);
    let bytes = serde_json::to_vec(&manifest()).unwrap();
    let signature = signer.sign(&bytes).to_bytes();
    let checked =
        verify_release_manifest(&bytes, &signature, signer.verifying_key().as_bytes()).unwrap();
    assert!(checked.readable_storage.contains(1, 3));
    assert!(!checked.writable_storage.contains(1, 3));
    assert!(checked.writable_storage.contains(1, 4));
    assert!(matches!(
        verify_release_manifest(&bytes, &signature, other.verifying_key().as_bytes()),
        Err(ReleaseManifestError::Signature)
    ));
    let mut tampered = bytes;
    tampered.push(b' ');
    assert!(matches!(
        verify_release_manifest(&tampered, &signature, signer.verifying_key().as_bytes()),
        Err(ReleaseManifestError::Signature)
    ));
}

/// Authenticity does not make traversal, ambiguous JSON, or invalid versions valid.
#[test]
fn signed_unsafe_paths_and_incompatible_ranges_are_rejected() {
    let signer = SigningKey::from_bytes(&[7; 32]);
    let mut value = manifest();
    value["artifacts"][0]["path"] = "bin/../jumpstart-server".into();
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        verify_release_manifest(
            &bytes,
            &signer.sign(&bytes).to_bytes(),
            signer.verifying_key().as_bytes()
        ),
        Err(ReleaseManifestError::Invalid(_))
    ));

    let mut value = manifest();
    value["writable_storage"]["max_schema"] = 6.into();
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        verify_release_manifest(
            &bytes,
            &signer.sign(&bytes).to_bytes(),
            signer.verifying_key().as_bytes()
        ),
        Err(ReleaseManifestError::Invalid(_))
    ));

    let bytes = br#"{"manifest_version":1,"manifest_version":1}"#;
    assert!(matches!(
        verify_release_manifest(
            bytes,
            &signer.sign(bytes).to_bytes(),
            signer.verifying_key().as_bytes()
        ),
        Err(ReleaseManifestError::Decode(_))
    ));
}
