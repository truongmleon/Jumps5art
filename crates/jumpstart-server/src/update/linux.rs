//! Verifies a signed release bundle without staging it or changing the active release.
//!
//! Trust starts with the root-owned key outside the candidate directory. After
//! verifying its signature, the planner opens only the signed file list and
//! checks exact sizes and fingerprints through descriptor-relative reads.

use std::{
    error::Error,
    fs::File,
    io::{Read, copy},
    os::unix::fs::MetadataExt,
    path::Path,
};

use jumpstart_domain::BlobId;
use jumpstart_protocol::{MAX_FRAME_BYTES, release::verify_release_manifest};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
use serde_json::{Value, json};

const KEY_DIRECTORY: &str = "/etc/jumpstart";

/// Check a candidate against the live daemon's observed storage versions.
///
/// Returns a read-only report. A recovery-required root or invalid package fails
/// before any activation or filesystem mutation can be requested.
pub(super) fn plan(root: &Path, socket: &Path) -> Result<Value, Box<dyn Error>> {
    let status = crate::admin::status(socket)?;
    if status["state"] != "metadata_verified" {
        return Err("verify or recover metadata before planning an update".into());
    }
    let format = storage_version(&status["metadata"]["format_version"])?;
    let schema = storage_version(&status["metadata"]["schema_version"])?;
    let key_directory = open_directory(Path::new(KEY_DIRECTORY))?;
    let key_file = open_regular(&key_directory, "release.pub")?;
    for entry in [key_directory.metadata()?, key_file.metadata()?] {
        if entry.uid() != 0 || entry.mode() & 0o022 != 0 {
            return Err(
                "the installed release trust key and directory must be controlled by root".into(),
            );
        }
    }
    let key = fixed_bytes::<32>(key_file)?;
    inspect_bundle(root, &key, format, schema)
}

/// Verify all listed artifacts relative to one opened release directory.
///
/// `key` is the installed release key and the version values come from the
/// current daemon. Returns authenticated metadata and compatibility evidence.
/// A later apply step must reverify the package at its own staging boundary.
fn inspect_bundle(
    root: &Path,
    key: &[u8; 32],
    format: u32,
    schema: u32,
) -> Result<Value, Box<dyn Error>> {
    let directory = open_directory(root)?;
    let manifest_file = open_regular(&directory, "release.json")?;
    let mut bytes = Vec::new();
    manifest_file
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let signature = fixed_bytes::<64>(open_regular(&directory, "release.sig")?)?;
    let manifest = verify_release_manifest(&bytes, &signature, key)?;
    for artifact in &manifest.artifacts {
        let file = open_regular(&directory, &artifact.path)?;
        let expected = artifact.bytes.value();
        if file.metadata()?.len() != expected {
            return Err(format!("release artifact has the wrong length: {}", artifact.path).into());
        }
        let mut hasher = blake3::Hasher::new();
        let count = copy(&mut file.take(expected + 1), &mut hasher)?;
        if count != expected || BlobId::from_bytes(*hasher.finalize().as_bytes()) != artifact.hash {
            return Err(format!("release artifact did not verify: {}", artifact.path).into());
        }
    }
    Ok(json!({
        "scope": "verified_update_preflight",
        "release_id": manifest.release_id,
        "manifest_hash": BlobId::hash_bytes(&bytes),
        "artifacts_verified": manifest.artifacts.len(),
        "current_storage": {"format_version": format, "schema_version": schema},
        "can_read_current_storage": manifest.readable_storage.contains(format, schema),
        "can_write_current_storage": manifest.writable_storage.contains(format, schema),
        "protocol_min": manifest.protocol_min,
        "protocol_max": manifest.protocol_max,
        "migration_ids": manifest.migration_ids,
        "recovery_tool_version": manifest.recovery_tool_version,
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    /// Verify a real signed package, then reject changed bytes and symlink substitution.
    #[test]
    fn bundle_checks_exact_files_beneath_the_opened_directory() {
        let signer = SigningKey::from_bytes(&[9; 32]);
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("bin")).unwrap();
        let binary = fixture.path().join("bin/jumpstart-server");
        fs::write(&binary, b"abc").unwrap();
        let manifest = serde_json::to_vec(&json!({
            "manifest_version": 1,
            "release_id": "fixture",
            "protocol_min": {"major": 1, "minor": 0},
            "protocol_max": {"major": 1, "minor": 0},
            "readable_storage": {"min_format": 1, "max_format": 1, "min_schema": 4, "max_schema": 4},
            "writable_storage": {"min_format": 1, "max_format": 1, "min_schema": 4, "max_schema": 4},
            "migration_ids": [],
            "recovery_tool_version": "fixture",
            "artifacts": [{"path": "bin/jumpstart-server", "bytes": "3", "hash": BlobId::hash_bytes(b"abc")}]
        })).unwrap();
        fs::write(fixture.path().join("release.json"), &manifest).unwrap();
        fs::write(
            fixture.path().join("release.sig"),
            signer.sign(&manifest).to_bytes(),
        )
        .unwrap();
        let key = signer.verifying_key();
        let report = inspect_bundle(fixture.path(), key.as_bytes(), 1, 4).unwrap();
        assert_eq!(report["artifacts_verified"], 1);
        assert_eq!(report["can_write_current_storage"], true);

        fs::write(&binary, b"bad").unwrap();
        assert!(inspect_bundle(fixture.path(), key.as_bytes(), 1, 4).is_err());
        fs::remove_file(&binary).unwrap();
        let elsewhere = fixture.path().join("otherwise-valid");
        fs::write(&elsewhere, b"abc").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &binary).unwrap();
        assert!(inspect_bundle(fixture.path(), key.as_bytes(), 1, 4).is_err());
    }
}

/// Read a version from the live daemon's checked status response.
fn storage_version(field: &Value) -> Result<u32, Box<dyn Error>> {
    field
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| "the daemon did not report a valid storage version".into())
}

/// Open an absolute directory while refusing symlinks in any path component.
fn open_directory(path: &Path) -> Result<File, Box<dyn Error>> {
    if !path.is_absolute() {
        return Err("use an absolute release directory".into());
    }
    Ok(File::from(openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )?))
}

/// Open a regular file beneath the directory without following links or mounts.
fn open_regular(directory: &File, relative: &str) -> Result<File, Box<dyn Error>> {
    let file = File::from(openat2(
        directory,
        relative,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )?);
    if !file.metadata()?.is_file() {
        return Err("expected a regular release file".into());
    }
    Ok(file)
}

/// Read exactly `N` bytes and refuse trailing data in keys and signatures.
fn fixed_bytes<const N: usize>(mut file: File) -> Result<[u8; N], Box<dyn Error>> {
    let mut value = [0; N];
    file.read_exact(&mut value)?;
    if file.read(&mut [0])? != 0 {
        return Err("release key or signature has trailing bytes".into());
    }
    Ok(value)
}
