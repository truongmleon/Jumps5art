//! Runs the metadata command-line program and checks its observable results.
//!
//! For example, init without --development must exit with an error and leave the
//! requested folder absent. A successful init followed by inspect must print the
//! same server ID in its JSON report. Snapshot creation must require the same
//! development acknowledgment, preserve existing destinations, and report its
//! limited scope. Verification must work after the source is removed and fail
//! without modifying a damaged bundle. Existing files must remain untouched.
//! Migration also requires acknowledgment before storage access. An already-current
//! root must report a no-op without creating or changing a recovery snapshot.
//! Job tests cross process boundaries: enqueue must retain a request, running it
//! must create a verified snapshot, and later inspection must read its saved result.
//! Failed work must be visible in the saved record and in the command's exit code.
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

/// Initialize a source root through the CLI for a local metadata command test.
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
    assert!(error.contains("local metadata"), "{error}");
    assert!(output.stdout.is_empty());
    assert!(!missing_parent.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Verify creation reports the exact metadata scope and preserves authority IDs;
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
    assert_eq!(created["scope"], "metadata");
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

/// Require acknowledgment before migration can inspect unavailable paths or create
/// a missing parent. Assertion or fixture failures panic.
#[test]
fn migration_requires_acknowledgment_before_storage_io() {
    let parent = TempDir::new().unwrap();
    let missing_parent = parent.path().join("missing-parent");
    let source = missing_parent.join("source");
    let snapshot = missing_parent.join("before-upgrade");
    let output = command()
        .args(["migrate", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--development"), "{error}");
    assert!(error.contains("migration"), "{error}");
    assert!(output.stdout.is_empty());
    assert!(!missing_parent.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Verify migration of current schema 3 reports a no-op, preserves identity and
/// database bytes, and does not create a recovery artifact. Fixture failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn migration_of_current_schema_reports_json_noop_without_creating_snapshot() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    let snapshot = parent.path().join("unused-snapshot");
    let initial = initialize_source(&source);
    assert_eq!(initial["schema_version"], 3);
    let marker_before = fs::read(source.join("format.json")).unwrap();
    let database_before = fs::read(source.join("database/jumpstart.sqlite")).unwrap();
    let output = command()
        .args(["migrate", "--development", "--json", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["metadata"], initial);
    assert_eq!(report["from_schema_version"], 3);
    assert_eq!(report["to_schema_version"], 3);
    assert_eq!(report["backup"], Value::Null);
    assert_eq!(report["resumed"], false);
    assert_eq!(fs::read(source.join("format.json")).unwrap(), marker_before);
    assert_eq!(
        fs::read(source.join("database/jumpstart.sqlite")).unwrap(),
        database_before
    );
    assert!(!source.join("migration.json").exists());
    assert!(!snapshot.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
}

/// Verify a current root does not adopt or overwrite an existing snapshot-output
/// directory when migration has nothing to do. Assertion or fixture failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn migration_noop_preserves_existing_snapshot_output() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    initialize_source(&source);
    let snapshot = parent.path().join("occupied-output");
    fs::create_dir(&snapshot).unwrap();
    let sentinel = snapshot.join("original.txt");
    fs::write(&sentinel, b"preserve occupied output").unwrap();
    let output = command()
        .args(["migrate", "--development", "--json", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["backup"], Value::Null);
    assert_eq!(fs::read(sentinel).unwrap(), b"preserve occupied output");
    assert_eq!(fs::read_dir(snapshot).unwrap().count(), 1);
}

/// Keep migration's human-readable no-op report explicit about versions and the
/// absent recovery snapshot. Assertion or fixture failures panic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn migration_noop_text_reports_versions_and_absent_backup() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    initialize_source(&source);
    let snapshot = parent.path().join("unused-snapshot");
    let output = command()
        .args(["migrate", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("Verified offline development metadata migration:\n"));
    assert!(text.contains("  from_schema_version: 3\n"));
    assert!(text.contains("  to_schema_version: 3\n"));
    assert!(text.contains("  backup: null\n"));
    assert!(text.contains("  resumed: false\n"));
    assert!(!snapshot.exists());
}

/// Refuse a missing source without creating a replacement root or snapshot.
/// Assertion or fixture failures panic.
#[test]
fn migration_of_missing_root_does_not_initialize_it() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("missing-source");
    let snapshot = parent.path().join("snapshot");
    let output = command()
        .args(["migrate", "--development", "--json", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!source.exists());
    assert!(!snapshot.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Refuse an unrelated existing directory and leave its contents untouched rather
/// than adopting it as an old metadata root. Assertion or fixture failures panic.
#[test]
fn migration_does_not_adopt_existing_user_content() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("existing-folder");
    fs::create_dir(&source).unwrap();
    let sentinel = source.join("original.txt");
    fs::write(&sentinel, b"keep unrelated contents").unwrap();
    let snapshot = parent.path().join("snapshot");
    let output = command()
        .args(["migrate", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(sentinel).unwrap(), b"keep unrelated contents");
    assert_eq!(fs::read_dir(source).unwrap().count(), 1);
    assert!(!snapshot.exists());
}

/// Require unsupported platforms to fail explicitly before creating either path.
/// Assertion or fixture failures panic.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn migration_reports_unsupported_platform_without_creating_paths() {
    let parent = TempDir::new().unwrap();
    let source = parent.path().join("source");
    let snapshot = parent.path().join("snapshot");
    let output = command()
        .args(["migrate", "--development", "--metadata-root"])
        .arg(&source)
        .arg("--snapshot-output")
        .arg(&snapshot)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("supported only on Linux and macOS"));
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Enqueue a real snapshot request through the executable without running it.
///
/// `root` is an initialized metadata `&Path`; `output` is the new snapshot `&Path`.
/// Returns its JSON `Value`. Panics on process, command, or decoding failure.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn enqueue_snapshot(root: &Path, output: &Path) -> Value {
    let result = command()
        .args([
            "jobs",
            "enqueue-snapshot",
            "--development",
            "--json",
            "--metadata-root",
        ])
        .arg(root)
        .arg("--output")
        .arg(output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

/// Read one persisted job through a fresh process, requiring no acknowledgment.
///
/// `root` is the initialized `&Path`; `id` is the job's UUID `&str`. Returns its
/// JSON `Value`; panics if the command fails or produces invalid JSON.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_snapshot_job(root: &Path, id: &str) -> Value {
    let result = command()
        .args(["jobs", "inspect", "--json", "--metadata-root"])
        .arg(root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

/// Check every job-writing command acknowledges the preview before opening a root.
/// Valid IDs/revisions avoid making argument parsing the reason for refusal.
#[test]
fn job_writes_require_acknowledgment_before_storage_io() {
    let parent = TempDir::new().unwrap();
    let missing = parent.path().join("missing-root");
    let output = parent.path().join("snapshot");
    for action in ["enqueue-snapshot", "run", "cancel", "recover"] {
        let mut invocation = command();
        invocation
            .args(["jobs", action, "--metadata-root"])
            .arg(&missing);
        if action == "enqueue-snapshot" {
            invocation.arg("--output").arg(&output);
        }
        if matches!(action, "run" | "cancel") {
            invocation.args(["--job-id", "019c2039-4c6b-7000-8000-000000000001"]);
        }
        if action == "cancel" {
            invocation.args(["--expected-revision", "1"]);
        }
        let result = invocation.output().unwrap();
        assert!(!result.status.success(), "{action}");
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("--development"), "{action}: {error}");
        assert!(result.stdout.is_empty());
    }
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Retain a queued request across processes, and prove read commands do not alter
/// the metadata files or create the snapshot before execution.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn queued_jobs_persist_and_read_commands_leave_metadata_unchanged() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = parent.path().join("snapshot");
    let initial = initialize_source(&root);
    assert_eq!(initial["schema_version"], 3);
    let queued = enqueue_snapshot(&root, &output);
    let id = queued["job_id"].as_str().unwrap();
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["revision"], "1");
    assert_eq!(queued["attempt_generation"], "0");
    assert_eq!(queued["created_seq"], "1");
    assert!(queued["result"].is_null());
    assert!(queued["error"].is_null());
    assert!(!output.exists());
    let marker_before = fs::read(root.join("format.json")).unwrap();
    let database_before = fs::read(root.join("database/jumpstart.sqlite")).unwrap();

    assert_eq!(inspect_snapshot_job(&root, id), queued);
    let listed = command()
        .args(["jobs", "list", "--json", "--limit", "1", "--metadata-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let report: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(report["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(report["jobs"][0], queued);
    assert_eq!(fs::read(root.join("format.json")).unwrap(), marker_before);
    assert_eq!(
        fs::read(root.join("database/jumpstart.sqlite")).unwrap(),
        database_before
    );
    assert!(!output.exists());
}

/// Execute an accepted request and inspect its completed result in a later process.
/// Completion requires an actual snapshot that the independent verifier accepts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn running_a_snapshot_job_persists_verified_completion() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = parent.path().join("snapshot");
    let initial = initialize_source(&root);
    let queued = enqueue_snapshot(&root, &output);
    let id = queued["job_id"].as_str().unwrap();
    let run = command()
        .args(["jobs", "run", "--development", "--json", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let finished: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(finished["job_id"], id);
    assert_eq!(finished["state"], "completed");
    assert_eq!(finished["revision"], "4");
    assert_eq!(finished["attempt_generation"], "2");
    assert!(finished["error"].is_null());
    assert_eq!(finished["result"]["scope"], "metadata");
    assert_eq!(finished["result"]["server_id"], initial["server_id"]);
    assert_eq!(inspect_snapshot_job(&root, id), finished);
    let verified = command()
        .args(["snapshot", "verify", "--json", "--snapshot-root"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&verified.stdout).unwrap(),
        finished["result"]
    );

    let database_before = fs::read(output.join("metadata.sqlite")).unwrap();
    let repeated = command()
        .args(["jobs", "run", "--development", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert_eq!(
        fs::read(output.join("metadata.sqlite")).unwrap(),
        database_before
    );
    assert_eq!(inspect_snapshot_job(&root, id), finished);
}

/// A destination occupied after acceptance must preserve that data, save Failed,
/// print the saved outcome, and return a nonzero status to callers.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn failed_snapshot_job_records_outcome_and_exits_unsuccessfully() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = parent.path().join("snapshot");
    initialize_source(&root);
    let queued = enqueue_snapshot(&root, &output);
    let id = queued["job_id"].as_str().unwrap();
    fs::create_dir(&output).unwrap();
    let sentinel = output.join("original.txt");
    fs::write(&sentinel, b"keep occupied destination").unwrap();
    let run = command()
        .args(["jobs", "run", "--development", "--json", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(!run.status.success());
    let failed: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(failed["state"], "failed");
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert!(failed["result"].is_null());
    assert!(String::from_utf8_lossy(&run.stderr).contains("failed"));
    assert_eq!(inspect_snapshot_job(&root, id), failed);
    assert_eq!(fs::read(sentinel).unwrap(), b"keep occupied destination");
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
}

/// Cancellation uses the inspected revision and applies only to queued work;
/// rejected stale controls and later run requests cannot reopen a canceled job.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn queued_job_cancellation_checks_revision_and_prevents_execution() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = parent.path().join("snapshot");
    initialize_source(&root);
    let queued = enqueue_snapshot(&root, &output);
    let id = queued["job_id"].as_str().unwrap();
    let stale = command()
        .args([
            "jobs",
            "cancel",
            "--development",
            "--json",
            "--expected-revision",
            "2",
            "--metadata-root",
        ])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert_eq!(inspect_snapshot_job(&root, id), queued);
    let canceled = command()
        .args([
            "jobs",
            "cancel",
            "--development",
            "--json",
            "--expected-revision",
            "1",
            "--metadata-root",
        ])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(
        canceled.status.success(),
        "{}",
        String::from_utf8_lossy(&canceled.stderr)
    );
    let saved: Value = serde_json::from_slice(&canceled.stdout).unwrap();
    assert_eq!(saved["state"], "canceled");
    assert_eq!(saved["revision"], "2");
    assert_eq!(saved["attempt_generation"], "0");
    let attempted = command()
        .args(["jobs", "run", "--development", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(!attempted.status.success());
    assert_eq!(inspect_snapshot_job(&root, id), saved);
    assert!(!output.exists());
}

/// Recovery leaves queued work waiting and returns an explicit empty result rather
/// than treating an unexecuted request as failed or completed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn job_recovery_preserves_queued_requests() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    let output = parent.path().join("snapshot");
    initialize_source(&root);
    let queued = enqueue_snapshot(&root, &output);
    let id = queued["job_id"].as_str().unwrap();
    let recovered = command()
        .args([
            "jobs",
            "recover",
            "--development",
            "--json",
            "--metadata-root",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let report: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(report["jobs"], serde_json::json!([]));
    assert_eq!(inspect_snapshot_job(&root, id), queued);
    assert!(!output.exists());
}

/// Text-mode listing must display its job records rather than silently dropping
/// a top-level JSON array when writing a human-readable report.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn job_list_text_displays_saved_job_identity_and_state() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    initialize_source(&root);
    let queued = enqueue_snapshot(&root, &parent.path().join("snapshot"));
    let listed = command()
        .args(["jobs", "list", "--metadata-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(listed.status.success());
    let text = String::from_utf8(listed.stdout).unwrap();
    assert!(text.starts_with("Persisted local snapshot jobs:\n"));
    assert!(text.contains(queued["job_id"].as_str().unwrap()));
    assert!(text.contains("queued"));
}

/// List bounds and domain IDs are validated by the argument parser before any
/// missing metadata path can be opened or initialized.
#[test]
fn job_arguments_reject_invalid_limits_ids_and_zero_revisions() {
    let parent = TempDir::new().unwrap();
    let missing = parent.path().join("missing");
    for limit in ["0", "101"] {
        let result = command()
            .args(["jobs", "list", "--limit", limit, "--metadata-root"])
            .arg(&missing)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
    let invalid_id = command()
        .args([
            "jobs",
            "inspect",
            "--job-id",
            "not-a-uuid",
            "--metadata-root",
        ])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(invalid_id.status.code(), Some(2));
    let zero = command()
        .args([
            "jobs",
            "cancel",
            "--development",
            "--job-id",
            "019c2039-4c6b-7000-8000-000000000001",
            "--expected-revision",
            "0",
            "--metadata-root",
        ])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(zero.status.code(), Some(2));
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Read and write actions on missing roots must fail without creating a replacement
/// metadata database, an output folder, or an accepted job record.
#[test]
fn job_commands_do_not_initialize_missing_metadata() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("missing");
    let output = parent.path().join("snapshot");
    for action in [
        "enqueue-snapshot",
        "run",
        "list",
        "inspect",
        "cancel",
        "recover",
    ] {
        let mut invocation = command();
        invocation
            .args(["jobs", action, "--json", "--metadata-root"])
            .arg(&root);
        if matches!(action, "enqueue-snapshot" | "run" | "cancel" | "recover") {
            invocation.arg("--development");
        }
        if action == "enqueue-snapshot" {
            invocation.arg("--output").arg(&output);
        }
        if matches!(action, "run" | "inspect" | "cancel") {
            invocation.args(["--job-id", "019c2039-4c6b-7000-8000-000000000001"]);
        }
        if action == "cancel" {
            invocation.args(["--expected-revision", "1"]);
        }
        let result = invocation.output().unwrap();
        assert!(!result.status.success(), "{action}");
        assert!(result.stdout.is_empty(), "{action}");
    }
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Unsupported platforms refuse the job API explicitly without creating files,
/// even when an operator acknowledges the development preview.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn job_commands_report_unsupported_platform_without_creating_paths() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("missing");
    let result = command()
        .args([
            "jobs",
            "recover",
            "--development",
            "--json",
            "--metadata-root",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("supported only on Linux and macOS"));
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

/// Read historical values through fresh processes without repeating the saved work.
/// JSON keeps full projections; text provides a concise state/revision/token timeline.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn job_history_explains_completed_work_in_json_and_text_without_mutation() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    initialize_source(&root);
    let queued = enqueue_snapshot(&root, &parent.path().join("snapshot"));
    let id = queued["job_id"].as_str().unwrap();
    let run = command()
        .args(["jobs", "run", "--development", "--json", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let completed: Value = serde_json::from_slice(&run.stdout).unwrap();
    let database = root.join("database/jumpstart.sqlite");
    let before = fs::read(&database).unwrap();
    let history = command()
        .args(["jobs", "history", "--json", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stderr)
    );
    let report: Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(report["job_id"], id);
    let entries = report["history"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0], queued);
    assert_eq!(entries[3], completed);
    assert!(entries[..3].iter().all(|entry| entry["result"].is_null()));
    let text = command()
        .args(["jobs", "history", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", id])
        .output()
        .unwrap();
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.starts_with(&format!("Snapshot job {id} history:\n")));
    assert!(text.contains("worker token"));
    for state in ["queued", "running", "verifying", "completed"] {
        assert!(text.contains(state));
    }
    assert_eq!(fs::read(database).unwrap(), before);
    assert_eq!(inspect_snapshot_job(&root, id), completed);
}

/// Unknown jobs and missing roots are errors, without an empty success or initialization.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn job_history_refuses_unknown_or_missing_data_without_creating_paths() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("metadata");
    initialize_source(&root);
    let missing = parent.path().join("missing");
    for path in [&root, &missing] {
        let result = command()
            .args(["jobs", "history", "--json", "--metadata-root"])
            .arg(path)
            .args(["--job-id", "019c2039-4c6b-7000-8000-000000000001"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
    }
    assert!(!missing.exists());
}

/// Keep the history command recognizable while explicitly refusing unsupported storage.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn job_history_reports_unsupported_storage_without_creating_paths() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("missing");
    let result = command()
        .args(["jobs", "history", "--json", "--metadata-root"])
        .arg(&root)
        .args(["--job-id", "019c2039-4c6b-7000-8000-000000000001"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("supported only on Linux and macOS"));
    assert!(!root.exists());
}

/// Reads a capacity report's byte field and checks its exact decimal-string format.
///
/// # Arguments
///
/// * `report` — borrowed JSON `&Value` holding a report or its assessment object.
/// * `field` — borrowed `&str` naming a required byte-count field.
///
/// # Returns
///
/// The field's `u64` value, within the shared counter's nonnegative signed-64-bit
/// range and encoded without a sign, whitespace, or redundant leading zeroes.
///
/// # Panics
///
/// Panics if the field is absent, numeric JSON, malformed, or outside that range.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn capacity_bytes(report: &Value, field: &str) -> u64 {
    let text = report[field].as_str().expect("decimal-string byte field");
    let bytes = text.parse::<u64>().expect("unsigned byte count");
    assert_eq!(text, bytes.to_string());
    assert!(bytes <= i64::MAX as u64);
    bytes
}

/// Checks JSON scope, byte formats, and assessment consistency within one capacity
/// observation, preserving files and avoiding comparisons with a later host reading.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn storage_inspect_json_reports_capacity_only_without_changing_contents() {
    let root = TempDir::new().unwrap();
    let sentinel = root.path().join("keep.txt");
    let nested = root.path().join("nested");
    fs::write(&sentinel, b"original top-level contents").unwrap();
    fs::create_dir(&nested).unwrap();
    let nested_sentinel = nested.join("keep-too.txt");
    fs::write(&nested_sentinel, b"original nested contents").unwrap();

    let output = command()
        .args(["storage", "inspect"])
        .arg(root.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scope"], "capacity_only");
    let observed_path = Path::new(report["path"].as_str().expect("reported directory path"));
    assert_eq!(observed_path, fs::canonicalize(root.path()).unwrap());

    let total = capacity_bytes(&report, "total_bytes");
    let free = capacity_bytes(&report, "free_bytes");
    let available = capacity_bytes(&report, "available_bytes");
    assert!(available <= free);
    assert!(free <= total);
    let assessment = &report["assessment"];
    let warning = capacity_bytes(assessment, "warning_below_bytes");
    let reserve = capacity_bytes(assessment, "safety_reserve_bytes");
    let above_reserve = capacity_bytes(assessment, "available_above_reserve_bytes");
    assert!(warning <= total);
    assert!(reserve <= total);
    assert_eq!(above_reserve, available.saturating_sub(reserve));
    assert_eq!(assessment["below_warning_threshold"], available < warning);
    assert_eq!(
        assessment["at_or_below_safety_reserve"],
        available <= reserve
    );

    assert_eq!(fs::read(sentinel).unwrap(), b"original top-level contents");
    assert_eq!(
        fs::read(nested_sentinel).unwrap(),
        b"original nested contents"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    assert_eq!(fs::read_dir(nested).unwrap().count(), 1);
}

/// Checks the human-readable heading distinguishes a capacity observation from
/// disk health or write permission, without requiring development acknowledgment.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn storage_inspect_text_labels_the_limited_observation() {
    let root = TempDir::new().unwrap();
    let output = command()
        .args(["storage", "inspect"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("Storage capacity observation (not disk health or write approval):\n")
    );
    assert!(text.contains("  scope: capacity_only\n"));
    for field in ["total_bytes", "free_bytes", "available_bytes"] {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(&format!("  {field}: ")))
            .expect("labeled capacity field");
        let bytes = line.parse::<u64>().expect("text byte count");
        assert_eq!(line, bytes.to_string());
    }
    assert!(text.contains("  assessment:\n"));
    assert!(text.contains("    available_above_reserve_bytes: "));
    assert!(text.contains("    below_warning_threshold: "));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

/// Requires missing paths and regular files to fail without creating a directory,
/// replacing the file, or reporting a successful capacity observation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn storage_inspect_refuses_missing_paths_and_regular_files() {
    let parent = TempDir::new().unwrap();
    let missing = parent.path().join("missing");
    let file = parent.path().join("existing-file");
    fs::write(&file, b"keep existing file").unwrap();
    for path in [&missing, &file] {
        let output = command()
            .args(["storage", "inspect"])
            .arg(path)
            .arg("--json")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert!(!missing.exists());
    assert_eq!(fs::read(file).unwrap(), b"keep existing file");
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
}

/// Rejects a final directory symlink rather than silently observing its target,
/// leaving both the link and the target directory's contents untouched.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn storage_inspect_refuses_a_final_symlink() {
    let parent = TempDir::new().unwrap();
    let target = parent.path().join("target");
    fs::create_dir(&target).unwrap();
    let sentinel = target.join("original.txt");
    fs::write(&sentinel, b"keep symlink target").unwrap();
    let alias = parent.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let output = command()
        .args(["storage", "inspect"])
        .arg(&alias)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(
        fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(alias).unwrap(), target);
    assert_eq!(fs::read(sentinel).unwrap(), b"keep symlink target");
    assert_eq!(fs::read_dir(target).unwrap().count(), 1);
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 2);
}

/// Requires unsupported platforms, including Windows, to report the boundary
/// explicitly without changing the inspected directory or creating metadata.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn storage_inspect_reports_unsupported_platform_without_filesystem_changes() {
    let root = TempDir::new().unwrap();
    let sentinel = root.path().join("keep.txt");
    fs::write(&sentinel, b"keep unsupported-platform contents").unwrap();
    let output = command()
        .args(["storage", "inspect"])
        .arg(root.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("supported only on Linux and macOS"));
    assert_eq!(
        fs::read(sentinel).unwrap(),
        b"keep unsupported-platform contents"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
