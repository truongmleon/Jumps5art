//! Tests copying Jumpstart's server bookkeeping into a verified metadata snapshot.
//!
//! For example, a snapshot must retain the server's ID and still verify after the
//! original folder is gone. It must not copy private credentials or diagnostic
//! files, change the source, overwrite an existing destination, or accept damaged
//! or unfinished output as complete. These tests use real temporary SQLite files;
//! they do not exercise media backups, a running daemon, or restoring a server.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::SystemTime;

use fs2::FileExt;
use jumpstart_domain::{BlobId, ServerId};
use jumpstart_storage::{
    DATABASE_DIRECTORY, DATABASE_FILE, LOCK_FILE, MetadataReport, SNAPSHOT_DATABASE_FILE,
    SNAPSHOT_FORMAT_VERSION, SNAPSHOT_MANIFEST_FILE, SnapshotScope, StorageError, create_snapshot,
    initialize, verify_snapshot,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Creates a source root beneath a temporary directory kept alive by its guard.
///
/// # Returns
/// `(TempDir, PathBuf, MetadataReport)` containing the guard, canonical source
/// path, and verified initialization report.
///
/// # Panics
/// Panics if fixture directory creation or initialization fails.
fn source_fixture() -> (TempDir, PathBuf, MetadataReport) {
    let parent = tempfile::tempdir().expect("temporary parent");
    let report = initialize(&parent.path().join("metadata")).expect("initialize source");
    let root = report.root.clone();
    (parent, root, report)
}

/// Creates a complete snapshot for tests that deliberately alter its files.
///
/// # Returns
/// `(TempDir, PathBuf)` containing the lifetime guard and published snapshot path.
///
/// # Panics
/// Panics if fixture initialization or snapshot creation fails.
fn snapshot_fixture() -> (TempDir, PathBuf) {
    let (parent, source, _) = source_fixture();
    let output = parent.path().join("snapshot");
    let report = create_snapshot(&source, &output).expect("create fixture snapshot");
    (parent, report.root)
}

/// Records an entry's contents and metadata to detect unintended filesystem changes.
///
/// `mode` includes Unix permission and entry-type bits; `contents` is present for
/// regular files, while `link_target` records a symlink without following it.
#[derive(Debug, Eq, PartialEq)]
struct EntryState {
    mode: u32,
    modified: SystemTime,
    contents: Option<Vec<u8>>,
    link_target: Option<PathBuf>,
}

/// Captures a fixture tree without following symbolic links.
///
/// # Arguments
/// - `root`: borrowed `&Path` of the tree, including the root entry itself.
///
/// # Returns
/// A `BTreeMap<PathBuf, EntryState>` keyed by paths relative to `root`.
///
/// # Panics
/// Panics if an entry, timestamp, regular file, or link cannot be read.
fn fingerprint(root: &Path) -> BTreeMap<PathBuf, EntryState> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = BTreeMap::new();
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata");
        let contents = metadata
            .is_file()
            .then(|| fs::read(&path).expect("fixture file contents"));
        let link_target = metadata
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&path).expect("fixture symlink target"));
        result.insert(
            path.strip_prefix(root)
                .expect("relative fixture path")
                .to_path_buf(),
            EntryState {
                mode: metadata.permissions().mode(),
                modified: metadata.modified().expect("fixture modification time"),
                contents,
                link_target,
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).expect("fixture directory entries") {
                pending.push(entry.expect("fixture directory entry").path());
            }
        }
    }
    result
}

/// Changes a complete fixture's JSON manifest to exercise validation failures.
///
/// # Arguments
/// - `root`: borrowed `&Path` of the snapshot bundle.
/// - `edit`: an `FnOnce(&mut Value)` applying one test-only manifest mutation.
///
/// # Returns
/// Unit (`()`) after writing the edited JSON.
///
/// # Panics
/// Panics if the manifest cannot be read, decoded, encoded, or written.
fn edit_manifest(root: &Path, edit: impl FnOnce(&mut Value)) {
    let path = root.join(SNAPSHOT_MANIFEST_FILE);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&path).expect("read manifest")).expect("decode manifest");
    edit(&mut manifest);
    fs::write(
        path,
        serde_json::to_vec_pretty(&manifest).expect("encode manifest"),
    )
    .expect("write altered manifest");
}

/// Requires verification failure while preserving every fixture entry exactly.
///
/// # Arguments
/// - `root`: borrowed `&Path` of an existing, deliberately invalid snapshot.
///
/// # Returns
/// The `StorageError` returned by verification, for further typed assertions.
///
/// # Panics
/// Panics if verification succeeds, modifies the tree, or fixture access fails.
fn rejected_without_mutation(root: &Path) -> StorageError {
    let before = fingerprint(root);
    let error = verify_snapshot(root).expect_err("invalid snapshot must fail");
    assert_eq!(before, fingerprint(root));
    error
}

/// Requires source rejection without publishing the requested snapshot directory.
///
/// # Arguments
/// - `source`: borrowed `&Path` of the existing invalid or busy source fixture.
/// - `output`: borrowed `&Path` of a nonexistent sibling destination.
///
/// # Returns
/// The `StorageError` from creation, after checking the source was unchanged.
///
/// # Panics
/// Panics if creation succeeds, publishes output, changes the source, or fixture access fails.
fn rejected_source_without_output(source: &Path, output: &Path) -> StorageError {
    let before = fingerprint(source);
    let error = create_snapshot(source, output).expect_err("source must be rejected");
    assert!(!output.exists(), "rejection must not publish a snapshot");
    assert_eq!(before, fingerprint(source));
    error
}

/// Verifies a standalone two-file bundle preserves identity and exact byte metadata,
/// remains readable without its source, and uses owner-only permissions.
#[test]
fn snapshot_preserves_authority_and_verifies_after_source_is_gone() {
    let (parent, source, initial) = source_fixture();
    let output = parent.path().join("snapshot");
    let created = create_snapshot(&source, &output).expect("create snapshot");
    assert_eq!(
        created.root,
        fs::canonicalize(&output).expect("canonical snapshot")
    );
    assert_eq!(created.snapshot_format_version, SNAPSHOT_FORMAT_VERSION);
    assert_eq!(created.scope, SnapshotScope::BootstrapMetadata);
    assert_eq!(created.server_id, initial.server_id);
    assert_eq!(created.sync_epoch, initial.sync_epoch);
    assert_eq!(created.format_version, initial.format_version);
    assert_eq!(created.schema_version, initial.schema_version);
    assert_eq!(
        created.change_seq, None,
        "schema 1 has no stored change sequence"
    );

    let database = fs::read(output.join(SNAPSHOT_DATABASE_FILE)).expect("snapshot database");
    assert_eq!(created.database_bytes.value(), database.len() as u64);
    assert_eq!(created.database_hash, BlobId::hash_bytes(&database));
    let mut entries = fs::read_dir(&output)
        .expect("bundle entries")
        .map(|entry| entry.expect("bundle entry").file_name())
        .collect::<Vec<_>>();
    entries.sort();
    let mut expected =
        [SNAPSHOT_MANIFEST_FILE, SNAPSHOT_DATABASE_FILE].map(std::ffi::OsString::from);
    expected.sort();
    assert_eq!(entries, expected);
    assert_eq!(
        fs::metadata(&output)
            .expect("bundle mode")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for name in expected {
        let path = output.join(name);
        let metadata = fs::symlink_metadata(path).expect("bundle file metadata");
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    fs::remove_dir_all(&source).expect("remove original source");
    let before = fingerprint(&output);
    let verified = verify_snapshot(&output).expect("verify independent bundle");
    assert_eq!(verified.server_id, created.server_id);
    assert_eq!(verified.sync_epoch, created.sync_epoch);
    assert_eq!(verified.database_hash, created.database_hash);
    assert_eq!(verified.database_bytes, created.database_bytes);
    assert_eq!(verified.change_seq, None);
    assert_eq!(before, fingerprint(&output));
}

/// Ensures snapshot creation leaves source contents, permissions, and timestamps
/// unchanged and excludes private secrets, diagnostics, and unrelated files.
#[test]
fn snapshot_creation_preserves_source_and_excludes_non_database_files() {
    let (parent, source, _) = source_fixture();
    fs::write(
        source.join("secrets/provider-key"),
        b"private fixture credential",
    )
    .expect("secret fixture");
    fs::write(
        source.join("diagnostics/private.log"),
        b"private fixture log",
    )
    .expect("diagnostic fixture");
    fs::write(source.join("sentinel.txt"), b"original source data").expect("sentinel fixture");
    let before = fingerprint(&source);
    let output = parent.path().join("snapshot");
    create_snapshot(&source, &output).expect("create snapshot");
    assert_eq!(before, fingerprint(&source));
    assert!(!output.join("secrets").exists());
    assert!(!output.join("diagnostics").exists());
    assert!(!output.join("sentinel.txt").exists());
    assert!(!output.join("format.json").exists());
    assert!(!output.join(LOCK_FILE).exists());
    verify_snapshot(&output).expect("bundle is still complete");
}

/// Checks that an empty directory, a regular file, and a dangling symlink are
/// existing destinations that creation must preserve rather than overwrite.
#[test]
fn existing_destinations_are_never_replaced() {
    let (parent, source, _) = source_fixture();
    for kind in ["directory", "file", "symlink"] {
        let output = parent.path().join(kind);
        match kind {
            "directory" => fs::create_dir(&output).expect("existing directory"),
            "file" => fs::write(&output, b"keep original destination").expect("existing file"),
            "symlink" => symlink(parent.path().join("absent"), &output).expect("dangling link"),
            _ => unreachable!(),
        }
        let before = fingerprint(&output);
        assert!(create_snapshot(&source, &output).is_err());
        assert_eq!(before, fingerprint(&output));
    }
}

/// Rejects a destination inside the source even when a symlink to the source is
/// used as its parent, preventing recursive or source-mutating snapshot placement.
#[test]
fn output_cannot_be_nested_in_source_or_hidden_behind_a_parent_alias() {
    let (parent, source, _) = source_fixture();
    let alias = parent.path().join("source-alias");
    symlink(&source, &alias).expect("source parent alias");
    for output in [
        source.join("nested-snapshot"),
        alias.join("aliased-snapshot"),
    ] {
        let before = fingerprint(&source);
        assert!(create_snapshot(&source, &output).is_err());
        assert!(!output.exists());
        assert_eq!(before, fingerprint(&source));
    }
}

/// Confirms missing sources or databases are not initialized by snapshot creation.
#[test]
fn missing_source_or_database_never_publishes_output() {
    let parent = tempfile::tempdir().expect("parent");
    let missing = parent.path().join("missing-source");
    let output = parent.path().join("missing-output");
    assert!(create_snapshot(&missing, &output).is_err());
    assert!(!missing.exists());
    assert!(!output.exists());

    let (parent, source, _) = source_fixture();
    fs::remove_file(source.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).expect("remove database");
    let output = parent.path().join("missing-database-output");
    rejected_source_without_output(&source, &output);
}

/// Ensures corrupt source bytes are rejected without repair or completed output.
#[test]
fn corrupt_source_is_preserved_and_not_snapshotted() {
    let (parent, source, _) = source_fixture();
    fs::write(
        source.join(DATABASE_DIRECTORY).join(DATABASE_FILE),
        b"not a SQLite database",
    )
    .expect("damage source fixture");
    rejected_source_without_output(&source, &parent.path().join("snapshot"));
}

/// Rejects extra source tables whose names resemble SQLite's reserved prefix,
/// preserving source bytes and publishing no snapshot for an unsupported schema.
#[test]
fn sqlite_like_source_table_is_rejected_without_publishing_output() {
    let (parent, source, _) = source_fixture();
    let connection = Connection::open(source.join(DATABASE_DIRECTORY).join(DATABASE_FILE))
        .expect("open fixture source");
    connection
        .execute_batch("CREATE TABLE sqliteXhidden(value TEXT) STRICT")
        .expect("add unsupported source table");
    connection
        .close()
        .expect("checkpoint and close fixture source");
    assert!(matches!(
        rejected_source_without_output(&source, &parent.path().join("snapshot")),
        StorageError::MigrationMismatch
    ));
}

/// Requires recovery for nonempty source journal sidecars instead of ignoring
/// their possible committed data or changing them during snapshot creation.
#[test]
fn uncheckpointed_source_journals_are_not_silently_ignored() {
    for suffix in ["-wal", "-journal"] {
        let (parent, source, _) = source_fixture();
        fs::write(
            source
                .join(DATABASE_DIRECTORY)
                .join(format!("{DATABASE_FILE}{suffix}")),
            b"uncheckpointed source fixture",
        )
        .expect("source journal");
        assert!(matches!(
            rejected_source_without_output(&source, &parent.path().join("snapshot")),
            StorageError::NeedsRecovery(_)
        ));
    }
}

/// Uses a real committed WAL to check that a snapshot cannot silently omit newer
/// source changes, even when a writer bypasses the cooperating maintenance lock.
#[test]
fn live_source_wal_is_rejected_without_modifying_its_bytes() {
    let (parent, source, _) = source_fixture();
    let writer = Connection::open(source.join(DATABASE_DIRECTORY).join(DATABASE_FILE))
        .expect("open fixture writer");
    writer
        .pragma_update(None, "wal_autocheckpoint", 0)
        .expect("retain WAL");
    writer
        .execute(
            "UPDATE schema_migrations SET applied_at = 'live-wal-fixture'",
            [],
        )
        .expect("commit fixture change");
    assert!(matches!(
        rejected_source_without_output(&source, &parent.path().join("snapshot")),
        StorageError::NeedsRecovery(_)
    ));
    writer.close().expect("close fixture writer");
}

/// Confirms an exclusive maintenance lock prevents copying a source that may be
/// changing, and that releasing the lock permits a later snapshot attempt.
#[test]
fn active_writer_lock_blocks_snapshot_creation() {
    let (parent, source, _) = source_fixture();
    let lock = File::open(source.join(LOCK_FILE)).expect("maintenance lock");
    FileExt::try_lock_exclusive(&lock).expect("hold writer lock");
    let output = parent.path().join("snapshot");
    assert!(matches!(
        rejected_source_without_output(&source, &output),
        StorageError::InUse
    ));
    drop(lock);
    create_snapshot(&source, &output).expect("snapshot after writer releases lock");
}

/// Checks altered database bytes cannot pass verification using an unchanged
/// manifest, and that verification does not repair the altered bundle.
#[test]
fn tampered_snapshot_database_is_rejected_without_repair() {
    let (_parent, root) = snapshot_fixture();
    let mut file = OpenOptions::new()
        .append(true)
        .open(root.join(SNAPSHOT_DATABASE_FILE))
        .expect("open snapshot database");
    file.write_all(b"tampered trailing bytes")
        .expect("tamper database");
    file.sync_all().expect("sync fixture tampering");
    drop(file);
    rejected_without_mutation(&root);
}

/// Checks validly encoded but inconsistent manifest hashes, identities, and change
/// sequences are rejected; schema 1 has no stored sequence, including zero.
#[test]
fn inconsistent_manifest_values_do_not_override_database_verification() {
    for (field, value) in [
        (
            "database_hash",
            json!(BlobId::hash_bytes(b"wrong content").to_string()),
        ),
        ("server_id", json!(ServerId::new().to_string())),
        ("change_seq", json!("0")),
    ] {
        let (_parent, root) = snapshot_fixture();
        edit_manifest(&root, |manifest| manifest[field] = value);
        rejected_without_mutation(&root);
    }
}

/// Requires the manifest to explicitly declare that no change sequence is available,
/// instead of accepting a missing field as an implicit default.
#[test]
fn missing_change_sequence_is_rejected_without_mutation() {
    let (_parent, root) = snapshot_fixture();
    edit_manifest(&root, |manifest| {
        manifest
            .as_object_mut()
            .expect("manifest object")
            .remove("change_seq");
    });
    assert!(matches!(
        rejected_without_mutation(&root),
        StorageError::InvalidSnapshotManifest(_)
    ));
}

/// Checks identity and schema independently of the byte fingerprint by making
/// valid SQLite edits, including a misleading `sqliteXhidden` table name, and
/// updating the manifest to match the altered bytes.
#[test]
fn matching_digest_does_not_hide_database_identity_or_schema_tampering() {
    for mutation in ["identity", "schema", "sqlite_like_schema"] {
        let (_parent, root) = snapshot_fixture();
        let database_path = root.join(SNAPSHOT_DATABASE_FILE);
        let connection = Connection::open(&database_path).expect("open snapshot fixture");
        match mutation {
            "identity" => {
                let other_server = ServerId::new();
                connection
                    .execute(
                        "UPDATE server_state SET server_id = ?1 WHERE singleton = 1",
                        [other_server.as_uuid().as_bytes().as_slice()],
                    )
                    .expect("alter captured server identity");
            }
            "schema" => {
                connection
                    .execute_batch("CREATE TABLE unexpected_snapshot_table(value TEXT) STRICT")
                    .expect("alter captured schema");
            }
            "sqlite_like_schema" => {
                connection
                    .execute_batch("CREATE TABLE sqliteXhidden(value TEXT) STRICT")
                    .expect("add a table resembling SQLite internals");
            }
            _ => unreachable!(),
        }
        connection.close().expect("close altered snapshot fixture");
        let database = fs::read(database_path).expect("altered snapshot bytes");
        edit_manifest(&root, |manifest| {
            manifest["database_bytes"] = json!((database.len() as u64).to_string());
            manifest["database_hash"] = json!(BlobId::hash_bytes(&database).to_string());
        });
        let error = rejected_without_mutation(&root);
        match mutation {
            "identity" => assert!(matches!(error, StorageError::IdentityMismatch)),
            "schema" | "sqlite_like_schema" => {
                assert!(matches!(error, StorageError::MigrationMismatch));
            }
            _ => unreachable!(),
        }
    }
}

/// Ensures malformed manifest JSON is reported as a manifest error without
/// rewriting the damaged file or attempting to recreate its missing values.
#[test]
fn malformed_manifest_is_rejected_without_repair() {
    let (_parent, root) = snapshot_fixture();
    fs::write(root.join(SNAPSHOT_MANIFEST_FILE), b"{unfinished fixture").expect("damage manifest");
    assert!(matches!(
        rejected_without_mutation(&root),
        StorageError::InvalidSnapshotManifest(_)
    ));
}

/// Requires a typed unsupported-format error rather than interpreting a future
/// snapshot format as the current one or changing the bundle.
#[test]
fn unsupported_snapshot_format_is_reported_without_mutation() {
    let (_parent, root) = snapshot_fixture();
    edit_manifest(&root, |manifest| {
        manifest["snapshot_format_version"] = json!(999)
    });
    assert!(matches!(
        rejected_without_mutation(&root),
        StorageError::UnsupportedSnapshotFormat { found: 999, .. }
    ));
}

/// Verifies the exact bundle boundary rejects extra files and nonempty journal
/// sidecars instead of treating them as harmless or recovering their contents.
#[test]
fn extra_files_and_nonempty_snapshot_journals_are_rejected() {
    for name in [
        "unlisted.txt".to_owned(),
        format!("{SNAPSHOT_DATABASE_FILE}-wal"),
        format!("{SNAPSHOT_DATABASE_FILE}-journal"),
    ] {
        let (_parent, root) = snapshot_fixture();
        fs::write(root.join(name), b"unlisted fixture bytes").expect("extra bundle file");
        rejected_without_mutation(&root);
    }
}

/// Rejects symbolic-link journal entries without following or changing their
/// targets, preserving both the invalid bundle and the external fixture file.
#[test]
fn snapshot_journal_symlinks_are_rejected_without_touching_targets() {
    for suffix in ["-wal", "-journal"] {
        let (parent, root) = snapshot_fixture();
        let target = parent.path().join("external-journal");
        fs::write(&target, b"external fixture data").expect("external target");
        let target_before = fingerprint(&target);
        symlink(
            &target,
            root.join(format!("{SNAPSHOT_DATABASE_FILE}{suffix}")),
        )
        .expect("journal symlink");
        rejected_without_mutation(&root);
        assert_eq!(target_before, fingerprint(&target));
    }
}

/// Ensures missing bundle components and an empty working directory cannot be
/// mistaken for a complete snapshot or populated automatically by verification.
#[test]
fn partial_artifacts_never_verify_as_complete() {
    let (parent, complete) = snapshot_fixture();
    for (index, retained) in [
        None,
        Some(SNAPSHOT_MANIFEST_FILE),
        Some(SNAPSHOT_DATABASE_FILE),
    ]
    .into_iter()
    .enumerate()
    {
        let partial = parent.path().join(format!("partial-{index}"));
        fs::create_dir(&partial).expect("partial working directory");
        if let Some(name) = retained {
            fs::copy(complete.join(name), partial.join(name)).expect("one copied component");
        }
        rejected_without_mutation(&partial);
    }
}

/// Races snapshot creators targeting the same new path and requires one complete
/// winner, with no replacement or mixed output from losing attempts.
#[test]
fn concurrent_creators_publish_one_complete_snapshot() {
    let (parent, source, initial) = source_fixture();
    let output = parent.path().join("raced-snapshot");
    let barrier = Arc::new(Barrier::new(8));
    let tasks = (0..8)
        .map(|_| {
            let source = source.clone();
            let output = output.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                create_snapshot(&source, &output)
            })
        })
        .collect::<Vec<_>>();
    let outcomes = tasks
        .into_iter()
        .map(|task| task.join().expect("creator thread"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    let verified = verify_snapshot(&output).expect("complete winner");
    assert_eq!(verified.server_id, initial.server_id);
    assert_eq!(verified.sync_epoch, initial.sync_epoch);
    let winner = outcomes
        .into_iter()
        .find_map(std::result::Result::ok)
        .expect("one winner");
    assert_eq!(verified.database_hash, winner.database_hash);
    assert_eq!(verified.root, winner.root);
}
