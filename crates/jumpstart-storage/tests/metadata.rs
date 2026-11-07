//! Tests preserving files while creating or inspecting server bookkeeping data.
//!
//! For example, initializing a folder containing original.txt must refuse and leave
//! that file intact. Inspecting a missing database must fail without creating a new
//! one. A format.json copied from another server must not match this database.
//!
//! These tests use temporary folders and real SQLite files on Linux and macOS.
//! They compare contents, permissions, and modification times before/after inspection,
//! and exercise locks, unfinished database writes, and competing initializers.
//!
//! Both return values and the files left behind matter: reporting the expected error
//! would not be sufficient if the operation had changed existing data.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use fs2::FileExt;
use jumpstart_storage::{
    APPLICATION_ID, DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, LOCK_FILE, MetadataReport,
    StorageError, initialize, inspect,
};
use rusqlite::Connection;
use tempfile::TempDir;

mod support;

/// Create a temporary parent and initialize a fresh metadata root for a test.
///
/// # Returns
///
/// `(TempDir, PathBuf, MetadataReport)` containing the lifetime guard, root path, and
/// initial report. Keeping the `TempDir` alive preserves the fixture on disk.
///
/// # Panics
///
/// Panics if temporary-directory creation or metadata initialization fails.
fn fixture() -> (TempDir, PathBuf, MetadataReport) {
    let parent = tempfile::tempdir().expect("temporary parent");
    let root = parent.path().join("metadata");
    let report = initialize(&root).expect("initialize metadata");
    (parent, root, report)
}

/// Build the database file path inside a test root.
///
/// # Arguments
///
/// * `root` — `&Path` of the fixture's metadata root.
///
/// # Returns
///
/// An owned `PathBuf` naming the SQLite file without accessing the filesystem.
fn database(root: &Path) -> PathBuf {
    root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)
}

/// Apply SQL to a fixture and checkpoint the result before offline inspection.
///
/// # Arguments
///
/// * `root` — `&Path` of the initialized fixture root.
/// * `sql` — `&str` containing test-only SQL that changes the database.
///
/// # Panics
///
/// Panics if opening, modifying, checkpointing, or closing the fixture fails.
/// The function returns `()` after successful mutation.
fn change_database(root: &Path, sql: &str) {
    let connection = Connection::open(database(root)).expect("open fixture database");
    connection
        .execute_batch(sql)
        .expect("mutate fixture database");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint fixture");
    connection.close().expect("close fixture database");
}

/// Read, edit, and replace a fixture's JSON format marker.
///
/// # Arguments
///
/// * `root` — `&Path` of the initialized fixture root.
/// * `edit` — `impl FnOnce(&mut serde_json::Value)` that mutates the decoded marker.
///
/// # Panics
///
/// Panics on fixture read, JSON conversion, or write failure. Returns `()` after the
/// edited marker is written.
fn change_marker(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join(FORMAT_FILE);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read marker")).expect("decode marker");
    edit(&mut value);
    fs::write(
        path,
        serde_json::to_vec_pretty(&value).expect("serialize marker"),
    )
    .expect("write fixture marker");
}

/// Snapshot of one fixture entry used to detect inspection side effects.
///
/// `permissions` stores Unix mode bits as `u32`, `modified` stores `SystemTime`, and
/// `bytes: Option<Vec<u8>>` contains file contents or `None` for a directory.
#[derive(Debug, Eq, PartialEq)]
struct FileState {
    permissions: u32,
    modified: std::time::SystemTime,
    bytes: Option<Vec<u8>>,
}

/// Snapshot every entry beneath a fixture root for before/after comparison.
///
/// # Arguments
///
/// * `root` — `&Path` of the tree to traverse, including the root entry itself.
///
/// # Returns
///
/// A `BTreeMap<PathBuf, FileState>` keyed by paths relative to the root.
///
/// # Panics
///
/// Panics if fixture metadata, directory entries, timestamps, or files cannot be read.
fn fingerprint(root: &Path) -> BTreeMap<PathBuf, FileState> {
    /// Recursively record one fixture entry and its children in a fingerprint map.
    ///
    /// # Arguments
    ///
    /// * `root` — `&Path` stripped from each recorded path.
    /// * `path` — `&Path` of the current entry.
    /// * `state` — `&mut BTreeMap<PathBuf, FileState>` receiving the snapshots.
    ///
    /// # Panics
    ///
    /// Panics on fixture access failures or if a path lies outside `root`; returns `()`.
    fn visit(root: &Path, path: &Path, state: &mut BTreeMap<PathBuf, FileState>) {
        let metadata = fs::symlink_metadata(path).expect("read tree metadata");
        let bytes = if metadata.is_file() {
            Some(fs::read(path).expect("read tree file"))
        } else {
            None
        };
        state.insert(
            path.strip_prefix(root)
                .expect("relative path")
                .to_path_buf(),
            FileState {
                permissions: metadata.permissions().mode(),
                modified: metadata.modified().expect("modified time"),
                bytes,
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).expect("read tree directory") {
                visit(root, &entry.expect("tree entry").path(), state);
            }
        }
    }
    let mut state = BTreeMap::new();
    visit(root, root, &mut state);
    state
}

/// Verify initialization and reopening agree on identity, versions, integrity, and
/// owner-only root/database permissions. Assertion or fixture failures panic.
#[test]
fn initialized_metadata_reopens_with_same_identity_and_verified_schema() {
    let (_parent, root, initial) = fixture();
    let reopened = inspect(&root).expect("offline reopen");
    assert_eq!(initial, reopened);
    assert_eq!(reopened.format_version, 1);
    assert_eq!(reopened.schema_version, 4);
    assert_eq!(reopened.change_seq.map(|value| value.value()), Some(0));
    assert_eq!(reopened.journal_mode, "wal");
    assert_eq!(reopened.integrity_check, "ok");
    assert_eq!(
        fs::metadata(&root)
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(database(&root))
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// Read original-schema files without upgrading them or inventing a stored sequence.
#[test]
fn original_schema1_remains_readable_without_mutation() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("original-metadata");
    let original = support::create_schema1(&root);
    let before = fingerprint(&root);
    let inspected = inspect(&root).unwrap();
    assert_eq!(inspected, original);
    assert_eq!(inspected.schema_version, 1);
    assert_eq!(inspected.change_seq, None);
    assert_eq!(fingerprint(&root), before);
}

/// Preserve the actual schema-2 high-water mark, including the maximum exact counter.
#[test]
fn schema2_inspection_reads_the_stored_change_sequence() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("schema-two");
    support::create_schema2(&root);
    for sequence in [42, i64::MAX] {
        change_database(
            &root,
            &format!("UPDATE server_state SET change_seq = {sequence}"),
        );
        let before = fingerprint(&root);
        assert_eq!(
            inspect(&root)
                .unwrap()
                .change_seq
                .map(|value| value.value()),
            Some(sequence as u64)
        );
        assert_eq!(fingerprint(&root), before);
    }
}

/// Refuse missing, altered, or extra migration records even when the table is intact.
#[test]
fn schema2_requires_both_exact_sequential_migration_records() {
    for sql in [
        "DELETE FROM schema_migrations WHERE version = 1",
        "DELETE FROM schema_migrations WHERE version = 2",
        "UPDATE schema_migrations SET name = 'another-change' WHERE version = 2",
        "INSERT INTO schema_migrations VALUES (3, 'extra', 'unexpected', 'now')",
    ] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("schema-two");
        support::create_schema2(&root);
        change_database(&root, sql);
        let before = fingerprint(&root);
        assert!(matches!(
            inspect(&root),
            Err(StorageError::MigrationMismatch)
        ));
        assert_eq!(fingerprint(&root), before);
    }
}

/// Leave mismatched marker/database versions untouched for explicit recovery.
#[test]
fn supported_but_disagreeing_schema_versions_are_rejected_without_repair() {
    let (_parent, root, _) = fixture();
    change_marker(&root, |marker| marker["schema_version"] = 1.into());
    let before = fingerprint(&root);
    assert!(matches!(
        inspect(&root),
        Err(StorageError::MigrationMismatch)
    ));
    assert_eq!(fingerprint(&root), before);
}

/// Verify repeated offline inspection preserves the fixture's entry fingerprints,
/// including contents, permissions, and modification times. Failures panic.
#[test]
fn offline_inspection_does_not_create_modify_or_remove_any_tree_entry() {
    let (_parent, root, _) = fixture();
    let before = fingerprint(&root);
    inspect(&root).expect("first inspection");
    inspect(&root).expect("repeated inspection");
    assert_eq!(before, fingerprint(&root));
}

/// Verify initialization refuses both occupied and empty existing directories and
/// preserves their fingerprints. Assertion or fixture failures panic.
#[test]
fn existing_content_and_empty_directories_are_never_adopted() {
    let parent = tempfile::tempdir().expect("parent");
    for (name, with_content) in [("empty", false), ("occupied", true)] {
        let root = parent.path().join(name);
        fs::create_dir(&root).expect("existing root");
        if with_content {
            fs::write(root.join("mine.txt"), b"keep this").expect("user data");
        }
        let before = fingerprint(&root);
        assert!(matches!(
            initialize(&root),
            Err(StorageError::ExistingTarget(_))
        ));
        assert_eq!(before, fingerprint(&root));
    }
}

/// Verify a second initialization refuses an existing root while preserving its
/// contents and original verified identity. Assertion or fixture failures panic.
#[test]
fn initialized_root_is_not_replaced_or_reinitialized() {
    let (_parent, root, report) = fixture();
    let before = fingerprint(&root);
    assert!(matches!(
        initialize(&root),
        Err(StorageError::ExistingTarget(_))
    ));
    assert_eq!(before, fingerprint(&root));
    assert_eq!(report, inspect(&root).expect("unchanged root"));
}

/// Verify initialization refuses existing files and dangling symlinks while retaining
/// their contents or entry type. Assertion or fixture failures panic.
#[test]
fn files_and_dangling_symlinks_are_existing_targets() {
    let parent = tempfile::tempdir().expect("parent");
    let file = parent.path().join("file");
    fs::write(&file, b"original").expect("existing file");
    assert!(matches!(
        initialize(&file),
        Err(StorageError::ExistingTarget(_))
    ));
    assert_eq!(fs::read(file).expect("read original"), b"original");
    let link = parent.path().join("link");
    symlink(parent.path().join("missing"), &link).expect("dangling link");
    assert!(matches!(
        initialize(&link),
        Err(StorageError::ExistingTarget(_))
    ));
    assert!(
        fs::symlink_metadata(link)
            .expect("link retained")
            .file_type()
            .is_symlink()
    );
}

/// Verify offline inspection rejects a symlink used as the root or format marker.
/// Assertion or fixture failures panic.
#[test]
fn inspection_rejects_root_and_marker_symlinks() {
    let (parent, root, _) = fixture();
    let link = parent.path().join("linked-root");
    symlink(&root, &link).expect("root link");
    assert!(matches!(
        inspect(&link),
        Err(StorageError::InvalidPath { .. })
    ));
    let original = parent.path().join("original-format.json");
    fs::rename(root.join(FORMAT_FILE), &original).expect("move marker");
    symlink(&original, root.join(FORMAT_FILE)).expect("marker link");
    assert!(matches!(
        inspect(&root),
        Err(StorageError::InvalidPath { .. })
    ));
}

/// Verify inspecting a missing root or database fails without creating a root or
/// changing the remaining entries. Assertion or fixture failures panic.
#[test]
fn inspection_of_missing_paths_never_initializes_them() {
    let parent = tempfile::tempdir().expect("parent");
    let missing = parent.path().join("absent");
    assert!(inspect(&missing).is_err());
    assert!(!missing.exists());
    let (_other, root, _) = fixture();
    fs::remove_file(database(&root)).expect("remove fixture database");
    let before = fingerprint(&root);
    assert!(inspect(&root).is_err());
    assert_eq!(before, fingerprint(&root));
}

/// Verify future marker/database versions are rejected with typed errors and leave
/// fixture fingerprints unchanged. Assertion or fixture failures panic.
#[test]
fn future_format_and_schema_versions_fail_without_mutation() {
    for target in ["format_version", "schema_version"] {
        let (_parent, root, _) = fixture();
        change_marker(&root, |marker| marker[target] = 99.into());
        let before = fingerprint(&root);
        let error = inspect(&root).expect_err("future marker must fail");
        assert!(matches!(
            error,
            StorageError::UnsupportedFormat { found: 99, .. }
                | StorageError::UnsupportedSchema { found: 99, .. }
        ));
        assert_eq!(before, fingerprint(&root));
    }
    let (_parent, root, _) = fixture();
    change_database(&root, "PRAGMA user_version = 99");
    let before = fingerprint(&root);
    assert!(matches!(
        inspect(&root),
        Err(StorageError::UnsupportedSchema { found: 99, .. })
    ));
    assert_eq!(before, fingerprint(&root));
}

/// Verify an incorrect SQLite application ID or migration checksum prevents a
/// successful inspection. Assertion or fixture failures panic.
#[test]
fn wrong_application_identity_and_migration_checksums_are_rejected() {
    let (_parent, root, _) = fixture();
    change_database(&root, "PRAGMA application_id = 123");
    assert!(matches!(inspect(&root), Err(StorageError::Integrity(_))));
    change_database(
        &root,
        &format!(
            "PRAGMA application_id = {APPLICATION_ID}; UPDATE schema_migrations SET checksum = 'not-the-migration'"
        ),
    );
    assert!(matches!(
        inspect(&root),
        Err(StorageError::MigrationMismatch)
    ));
}

/// Verify extra schema objects, including names resembling SQLite internals,
/// are rejected with an unchanged migration record and source tree. Failures panic.
#[test]
fn unexpected_schema_changes_are_not_hidden_by_a_correct_migration_record() {
    for table in ["unexpected", "sqliteXhidden"] {
        let (_parent, root, _) = fixture();
        change_database(&root, &format!("CREATE TABLE {table} (value TEXT) STRICT"));
        let before = fingerprint(&root);
        assert!(matches!(
            inspect(&root),
            Err(StorageError::MigrationMismatch)
        ));
        assert_eq!(before, fingerprint(&root));
    }
}

/// Verify a marker copied from another root is rejected without further changes to
/// the crossed fixture. Assertion or fixture failures panic.
#[test]
fn crossed_marker_and_database_identities_are_rejected() {
    let (_parent, root, _) = fixture();
    let (_other_parent, other, _) = fixture();
    fs::copy(other.join(FORMAT_FILE), root.join(FORMAT_FILE)).expect("cross fixture identity");
    let before = fingerprint(&root);
    assert!(matches!(
        inspect(&root),
        Err(StorageError::IdentityMismatch)
    ));
    assert_eq!(before, fingerprint(&root));
}

/// Verify a damaged SQLite header is rejected and offline inspection leaves the
/// damaged fixture unchanged. Assertion or fixture failures panic.
#[test]
fn corrupted_database_is_not_repaired_by_inspection() {
    let (_parent, root, _) = fixture();
    let mut database = OpenOptions::new()
        .write(true)
        .open(database(&root))
        .expect("open fixture");
    database.seek(SeekFrom::Start(0)).expect("seek header");
    database
        .write_all(b"damaged database!")
        .expect("damage header");
    database.sync_all().expect("sync fixture damage");
    drop(database);
    let before = fingerprint(&root);
    assert!(matches!(inspect(&root), Err(StorageError::Integrity(_))));
    assert_eq!(before, fingerprint(&root));
}

/// Verify nonempty WAL or rollback-journal sidecars require recovery and are preserved
/// by inspection. Assertion or fixture failures panic.
#[test]
fn nonempty_wal_and_rollback_journals_require_explicit_recovery() {
    for suffix in ["-wal", "-journal"] {
        let (_parent, root, _) = fixture();
        fs::write(
            root.join(DATABASE_DIRECTORY)
                .join(format!("{DATABASE_FILE}{suffix}")),
            b"uncheckpointed fixture",
        )
        .expect("write fixture sidecar");
        let before = fingerprint(&root);
        assert!(matches!(
            inspect(&root),
            Err(StorageError::NeedsRecovery(_))
        ));
        assert_eq!(before, fingerprint(&root));
    }
}

/// Verify inspection refuses a real live WAL without changing it, and observes the
/// changed identity after the writer checkpoints on close. Failures panic.
#[test]
fn committed_live_wal_is_never_silently_ignored_or_modified() {
    let (_parent, root, _) = fixture();
    let writer = Connection::open(database(&root)).expect("open live writer");
    writer
        .pragma_update(None, "wal_autocheckpoint", 0)
        .expect("retain WAL");
    writer
        .execute("UPDATE server_state SET sync_epoch = randomblob(16)", [])
        .expect("commit update into WAL");
    let wal = root
        .join(DATABASE_DIRECTORY)
        .join(format!("{DATABASE_FILE}-wal"));
    assert!(fs::metadata(wal).expect("live WAL metadata").len() > 0);
    let before = fingerprint(&root);
    assert!(matches!(
        inspect(&root),
        Err(StorageError::NeedsRecovery(_))
    ));
    assert_eq!(before, fingerprint(&root));
    writer.close().expect("close live writer");
    // Once the real WAL has been checkpointed, the changed identity is visible;
    // it could not have been safely inspected by ignoring that WAL earlier.
    assert!(matches!(
        inspect(&root),
        Err(StorageError::IdentityMismatch)
    ));
}

/// Verify inspection returns `InUse` without mutation while an exclusive maintenance
/// lock is held and succeeds after the handle is dropped. Failures panic.
#[test]
fn exclusive_writer_lock_blocks_inspection() {
    let (_parent, root, _) = fixture();
    let writer = File::open(root.join(LOCK_FILE)).expect("open maintenance lock");
    FileExt::try_lock_exclusive(&writer).expect("simulate active writer");
    let before = fingerprint(&root);
    assert!(matches!(inspect(&root), Err(StorageError::InUse)));
    assert_eq!(before, fingerprint(&root));
    drop(writer);
    inspect(&root).expect("inspection after writer exits");
}

/// Race eight initializers and verify exactly one succeeds with the identity later
/// read from the complete published root. Assertion or thread/fixture failures panic.
#[test]
fn concurrent_initializers_publish_exactly_one_complete_root() {
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("raced-metadata");
    let barrier = Arc::new(Barrier::new(8));
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                initialize(&root)
            })
        })
        .collect();
    let outcomes: Vec<_> = attempts
        .into_iter()
        .map(|task| task.join().expect("initializer thread"))
        .collect();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    let actual = inspect(&root).expect("winner is complete");
    let winner = outcomes
        .into_iter()
        .find_map(std::result::Result::ok)
        .expect("one winner");
    assert_eq!(actual, winner);
}

/// Verify a new initialization leaves unrelated interrupted staging untouched and
/// publishes a separately verified root. Assertion or fixture failures panic.
#[test]
fn interrupted_staging_is_preserved_and_never_treated_as_an_existing_installation() {
    let parent = tempfile::tempdir().expect("parent");
    let interrupted = parent.path().join(".jumpstart-init-interrupted");
    fs::create_dir(&interrupted).expect("old staging");
    fs::write(interrupted.join("partial.sqlite"), b"partial output").expect("partial data");
    let before = fingerprint(&interrupted);
    let root = parent.path().join("new-metadata");
    initialize(&root).expect("independent new initialization");
    assert_eq!(before, fingerprint(&interrupted));
    inspect(&root).expect("published new metadata");
}

/// Verify URI-reserved characters in a root path are escaped so inspection succeeds
/// without changing the root. Assertion or fixture failures panic.
#[test]
fn uri_reserved_characters_in_paths_do_not_change_inspection_flags() {
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("metadata ?#%& space");
    let report = initialize(&root).expect("initialize escaped path");
    let before = fingerprint(&root);
    assert_eq!(inspect(&root).expect("inspect escaped path"), report);
    assert_eq!(before, fingerprint(&root));
}

/// Require the complete known ledger schema instead of accepting a partial upgrade.
#[test]
fn current_schema_requires_exact_ledger_and_catalog_definitions() {
    for sql in [
        "DROP INDEX jobs_queue",
        "ALTER TABLE jobs ADD COLUMN surprise TEXT",
        "CREATE INDEX unexpected_job_index ON jobs(updated_at_ms)",
        "DELETE FROM schema_migrations WHERE version=3",
        "DELETE FROM schema_migrations WHERE version=4",
        "DROP INDEX mass_live_sibling_names",
        "ALTER TABLE media_marker ADD COLUMN surprise TEXT",
    ] {
        let (_parent, root, _) = fixture();
        change_database(&root, sql);
        let before = fingerprint(&root);
        let result = inspect(&root);
        assert!(
            matches!(result, Err(StorageError::MigrationMismatch)),
            "{sql}: {result:?}"
        );
        assert_eq!(fingerprint(&root), before);
    }
}

/// Reject a high-water mark whose corresponding durable change row is missing.
#[test]
fn schema3_change_positions_require_a_contiguous_retained_range() {
    let (_parent, root, _) = fixture();
    change_database(&root, "UPDATE server_state SET change_seq=1");
    let before = fingerprint(&root);
    assert!(matches!(inspect(&root), Err(StorageError::Integrity(_))));
    assert_eq!(fingerprint(&root), before);
}
