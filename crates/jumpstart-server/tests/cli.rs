//! Integration tests that run the metadata CLI as a child process.
//! Checks the development acknowledgment, JSON reports, and preservation of existing
//! or missing paths; successful storage operations are tested on Linux and macOS.
//!
//! The CLI is also a process boundary: flag handling, exit status, and output are
//! observable to people and scripts. Running the binary checks those behaviors
//! alongside the storage library's validation.

use std::{fs, process::Command};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::Value;
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
