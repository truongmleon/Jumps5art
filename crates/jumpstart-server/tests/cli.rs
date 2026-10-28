//! Runs the metadata command-line program and checks its observable results.
//!
//! For example, init without --development must exit with an error and leave the
//! requested folder absent. A successful init followed by inspect must print the
//! same server ID in its JSON report. Snapshot creation must require the same
//! development acknowledgment, preserve existing destinations, and report its
//! limited scope. Verification must work after the source is removed and fail
//! without modifying a damaged bundle. Existing files must remain untouched.
//!
//! These tests check argument handling, exit status, reports, and filesystem results
//! together. Storage-library tests alone do not exercise that program boundary.
//! The successful storage cases run on Linux and macOS; these tests do not start
//! a network service or test library media features.

use std::{fs, process::Command};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::Value;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;
use tempfile::TempDir;

/// Create a process command targeting Cargo's built `jumpstart-server` test binary.
///
/// # Returns
///
/// A `std::process::Command` ready for arguments; no child process is started yet.
fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jumpstart-server"))
}

/// Verify initialization without `--development` fails, explains the acknowledgment,
/// and leaves the requested root absent. Assertion or fixture failures panic.
#[test]
fn initialization_requires_explicit_development_acknowledgment() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = command()
        .arg("init")
        .arg("--metadata-root")
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--development"));
    assert!(!root.exists());
}

/// Verify acknowledged initialization and offline inspection emit JSON reports with
/// the same string-encoded authority and epoch. Assertion or fixture failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn init_then_inspect_preserves_authority_and_reports_json() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = command()
        .args(["init", "--development", "--json", "--metadata-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let initial: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output = command()
        .args(["inspect", "--json", "--metadata-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inspected: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(initial["server_id"].is_string());
    assert!(initial["sync_epoch"].is_string());
    assert_eq!(initial["server_id"], inspected["server_id"]);
    assert_eq!(initial["sync_epoch"], inspected["sync_epoch"]);
}

/// Verify the CLI refuses an existing directory and preserves its sentinel contents
/// and entry count. Assertion or fixture failures panic.
#[test]
fn existing_content_is_not_adopted_or_changed() {
    let root = TempDir::new().unwrap();
    let sentinel = root.path().join("original.txt");
    fs::write(&sentinel, "keep this").unwrap();
    let output = command()
        .args(["init", "--development", "--metadata-root"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep this");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

/// Verify the CLI fails when inspecting a missing root and does not create it.
/// Assertion or fixture failures panic.
#[test]
fn inspecting_missing_storage_does_not_initialize_it() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("missing");
    let output = command()
        .args(["inspect", "--json", "--metadata-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.exists());
}

/// Initialize a source root through the CLI for a snapshot test.
///
/// # Arguments
///
/// * `root` — `&Path` of a new root beneath the test's temporary directory.
///
/// # Returns
///
/// The JSON `Value` containing the initialized root's verified identity.
///
/// # Panics
///
/// Panics if the process cannot run, initialization fails, or stdout is not JSON.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn initialize_source(root: &Path) -> Value {
    let output = command()
        .args(["init", "--development", "--json", "--metadata-root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Verify snapshot creation requires acknowledgment before touching unavailable
/// source/output paths or creating their missing parent. Fixture failures panic.
#[test]
fn snapshot_creation_requires_acknowledgment_before_storage_io() {
    let parent = TempDir::new().unwrap();
    let missing_parent = parent.path().join("missing-parent");
    let source = missing_parent.join("source");
    let destination = missing_parent.join("snapshot");
    let output = command()
        .args(["snapshot", "create", "--metadata-root"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--development"), "{error}");
    assert!(error.contains("bootstrap metadata"), "{error}");
    assert!(output.stdout.is_empty());
    assert!(!missing_parent.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Verify creation reports the exact bootstrap scope and preserves authority IDs;
/// readonly verification works after source deletion without rewriting the bundle.
/// Assertion or fixture failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn snapshot_create_and_verify_report_scope_and_survive_source_removal() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    let destination = parent.path().join("snapshot");
    let initial = initialize_source(&source);
    let output = command()
        .args([
            "snapshot",
            "create",
            "--development",
            "--json",
            "--metadata-root",
        ])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let created: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["scope"], "bootstrap_metadata");
    assert_eq!(created["snapshot_format_version"], 1);
    assert_eq!(created["server_id"], initial["server_id"]);
    assert_eq!(created["sync_epoch"], initial["sync_epoch"]);
    assert_eq!(created["format_version"], initial["format_version"]);
    assert_eq!(created["schema_version"], initial["schema_version"]);
    assert!(created.as_object().unwrap().contains_key("change_seq"));
    assert_eq!(created["change_seq"], "0");
    assert_eq!(
        created["database_bytes"],
        fs::metadata(destination.join("metadata.sqlite"))
            .unwrap()
            .len()
            .to_string()
    );
    let hash = created["database_hash"].as_str().unwrap();
    assert!(hash.starts_with("b3:"));
    assert_eq!(hash.len(), 67);
    assert!(
        hash[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let mut entries: Vec<_> = fs::read_dir(&destination)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    entries.sort();
    assert_eq!(entries, ["metadata.sqlite", "snapshot.json"]);
    let manifest_before = fs::read(destination.join("snapshot.json")).unwrap();
    let database_before = fs::read(destination.join("metadata.sqlite")).unwrap();
    fs::remove_dir_all(&source).unwrap();
    let output = command()
        .args(["snapshot", "verify", "--json", "--snapshot-root"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verified: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created, verified);
    assert_eq!(
        manifest_before,
        fs::read(destination.join("snapshot.json")).unwrap()
    );
    assert_eq!(
        database_before,
        fs::read(destination.join("metadata.sqlite")).unwrap()
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
    assert!(!source.exists());
}

/// Verify snapshot creation refuses an existing destination and preserves its
/// sentinel instead of adopting, clearing, or overwriting it. Failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn snapshot_creation_preserves_existing_destination_contents() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    initialize_source(&source);
    let destination = parent.path().join("occupied");
    fs::create_dir(&destination).unwrap();
    let sentinel = destination.join("original.txt");
    fs::write(&sentinel, b"keep existing destination").unwrap();
    let output = command()
        .args(["snapshot", "create", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(sentinel).unwrap(), b"keep existing destination");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
}

/// Verify an unavailable source produces a failed command and no completed output.
/// Assertion or fixture failures panic.
#[test]
fn snapshot_creation_from_missing_source_does_not_publish_output() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("missing-source");
    let destination = parent.path().join("snapshot");
    let output = command()
        .args(["snapshot", "create", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!source.exists());
    assert!(!destination.exists());
}

/// Verify a missing snapshot cannot be verified and is never initialized as a
/// replacement bundle. Assertion or fixture failures panic.
#[test]
fn verifying_missing_snapshot_does_not_create_it() {
    let parent = TempDir::new().unwrap();
    let destination = parent.path().join("missing-snapshot");
    let output = command()
        .args(["snapshot", "verify", "--json", "--snapshot-root"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!destination.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Verify corruption fails at the command boundary without publishing a success
/// report, modifying the damaged database, or rewriting its manifest. Failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn snapshot_verification_rejects_corruption_without_changing_bundle() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    let destination = parent.path().join("snapshot");
    initialize_source(&source);
    let output = command()
        .args(["snapshot", "create", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("not a full backup"));
    let manifest_before = fs::read(destination.join("snapshot.json")).unwrap();
    let database = destination.join("metadata.sqlite");
    let mut damaged = fs::read(&database).unwrap();
    damaged[0] ^= 0xff;
    fs::write(&database, &damaged).unwrap();
    let output = command()
        .args(["snapshot", "verify", "--json", "--snapshot-root"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(database).unwrap(), damaged);
    assert_eq!(
        fs::read(destination.join("snapshot.json")).unwrap(),
        manifest_before
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
}

/// Verify unimplemented storage platforms return an explicit unsupported error for
/// acknowledged creation and verification without creating filesystem entries.
/// Assertion or fixture failures panic.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn snapshot_commands_report_unsupported_platform_without_creating_paths() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    let destination = parent.path().join("snapshot");
    for mut command in [
        {
            let mut operation = command();
            operation
                .args(["snapshot", "create", "--development", "--metadata-root"])
                .arg(&source)
                .arg("--output")
                .arg(&destination);
            operation
        },
        {
            let mut operation = command();
            operation
                .args(["snapshot", "verify", "--snapshot-root"])
                .arg(&destination);
            operation
        },
    ] {
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("supported only on Linux and macOS")
        );
        assert!(output.stdout.is_empty());
    }
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}
