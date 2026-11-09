//! Checks upgrading existing server bookkeeping without losing its recovery path.
//!
//! For example, upgrading an old metadata folder must keep the same server ID and
//! leave credentials and diagnostic files alone. A verified copy of the old
//! database must remain usable afterward. An interrupted upgrade is resumed only
//! through its recorded journal, rather than allowing ordinary inspection to
//! mistake partially updated bookkeeping for a ready installation.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use fs2::FileExt;
    use jumpstart_storage::{
        DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, LOCK_FILE, MIGRATION_FILE, MetadataReport,
        SCHEMA_VERSION, SNAPSHOT_DATABASE_FILE, SnapshotReport, StorageError, create_snapshot,
        initialize, inspect, migrate, verify_snapshot,
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::support::create_schema1;

    /// Keeps an entry's data and metadata for before/after preservation assertions.
    #[derive(Debug, Eq, PartialEq)]
    struct Entry {
        /// Exact regular-file contents; absent for directories and links.
        contents: Option<Vec<u8>>,
        /// A link's destination recorded without opening that destination.
        target: Option<PathBuf>,
        /// Unix permission and entry-kind bits.
        mode: u32,
        /// Last modification time, which reading alone must not change.
        modified: SystemTime,
    }

    /// Capture an entry tree without following symlinks.
    ///
    /// # Arguments
    /// * `root` — `&Path` of the test-created entry or tree to record.
    ///
    /// # Returns
    /// A `BTreeMap<PathBuf, Entry>` keyed by paths relative to `root`.
    ///
    /// # Panics
    /// Panics if a fixture entry, content, link, or timestamp cannot be read.
    fn fingerprint(root: &Path) -> BTreeMap<PathBuf, Entry> {
        let mut pending = vec![root.to_path_buf()];
        let mut result = BTreeMap::new();
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).expect("fixture entry metadata");
            let contents = metadata
                .is_file()
                .then(|| fs::read(&path).expect("fixture contents"));
            let target = metadata
                .file_type()
                .is_symlink()
                .then(|| fs::read_link(&path).expect("fixture link target"));
            result.insert(
                path.strip_prefix(root)
                    .expect("relative entry")
                    .to_path_buf(),
                Entry {
                    contents,
                    target,
                    mode: metadata.permissions().mode(),
                    modified: metadata.modified().expect("entry modification time"),
                },
            );
            if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .expect("fixture directory")
                        .map(|entry| entry.expect("fixture child").path()),
                );
            }
        }
        result
    }

    /// Construct a genuine old-format root using the frozen schema-1 fixture.
    ///
    /// # Returns
    /// `(TempDir, PathBuf, MetadataReport)` with the temporary-parent guard, root,
    /// and verified old-format identity.
    ///
    /// # Panics
    /// Panics if fixture construction or verification fails.
    fn old_fixture() -> (TempDir, PathBuf, MetadataReport) {
        let parent = tempfile::tempdir().expect("temporary migration parent");
        let root = parent.path().join("metadata");
        let report = create_schema1(&root);
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.change_seq, None);
        (parent, root, report)
    }

    /// Build the path of the fixture's SQLite file without accessing it.
    ///
    /// # Arguments
    /// * `root` — `&Path` of a metadata fixture.
    ///
    /// # Returns
    /// An owned `PathBuf` naming that fixture's database.
    fn database(root: &Path) -> PathBuf {
        root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)
    }

    /// Simulate a persisted upgrade journal after its recovery snapshot exists.
    ///
    /// # Arguments
    /// * `root` — `&Path` of the source whose upgrade will be resumed.
    /// * `source` — `&MetadataReport` describing the original schema-1 identity.
    /// * `backup` — `&SnapshotReport` of the independently verified recovery copy.
    ///
    /// # Returns
    /// Unit (`()`) after writing the exact journal shape consumed by migration.
    ///
    /// # Panics
    /// Panics if the simulated journal cannot be serialized or written.
    fn write_journal(root: &Path, source: &MetadataReport, backup: &SnapshotReport) {
        let journal = json!({
            "journal_format_version": 6,
            "from_schema_version": source.schema_version,
            "to_schema_version": SCHEMA_VERSION,
            "server_id": source.server_id,
            "sync_epoch": source.sync_epoch,
            "backup": backup,
        });
        fs::write(
            root.join(MIGRATION_FILE),
            serde_json::to_vec_pretty(&journal).expect("encode upgrade journal"),
        )
        .expect("write simulated journal");
    }

    /// Reject an unsafe initial upgrade without changing the source or publishing output.
    ///
    /// # Arguments
    /// * `root` — `&Path` of an existing test root that must remain unchanged.
    /// * `output` — `&Path` of a nonexistent snapshot destination.
    ///
    /// # Returns
    /// The rejected operation's `StorageError` after preservation checks.
    ///
    /// # Panics
    /// Panics if migration succeeds, changes the source, or creates the destination.
    fn rejected_without_changes(root: &Path, output: &Path) -> StorageError {
        let before = fingerprint(root);
        let error = migrate(root, output).expect_err("migration must refuse this root");
        assert_eq!(fingerprint(root), before);
        assert!(!output.exists(), "refusal must not publish a backup");
        error
    }

    /// Upgrade old bookkeeping, preserve unrelated files, and retain both old and
    /// new independently verifiable snapshots after the source is removed.
    #[test]
    fn upgrade_preserves_identity_auxiliary_files_and_verified_recovery_copy() {
        let (parent, root, initial) = old_fixture();
        fs::write(
            root.join("secrets/key-record"),
            b"private recovery material",
        )
        .expect("private sentinel");
        fs::write(
            root.join("diagnostics/operator.log"),
            b"prior diagnostic evidence",
        )
        .expect("diagnostic sentinel");
        fs::write(root.join("operator-notes.txt"), b"leave these notes intact")
            .expect("root sentinel");
        fs::set_permissions(
            root.join("secrets/key-record"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("sentinel permissions");
        let secrets = fingerprint(&root.join("secrets"));
        let diagnostics = fingerprint(&root.join("diagnostics"));
        let notes = fingerprint(&root.join("operator-notes.txt"));
        let recovery_path = parent.path().join("before-upgrade");

        let result = migrate(&root, &recovery_path).expect("upgrade schema 1");

        assert_eq!(result.from_schema_version, 1);
        assert_eq!(result.to_schema_version, SCHEMA_VERSION);
        assert!(!result.resumed);
        assert_eq!(
            result.metadata,
            inspect(&root).expect("inspect upgraded root")
        );
        assert_eq!(result.metadata.schema_version, SCHEMA_VERSION);
        assert_eq!(result.metadata.server_id, initial.server_id);
        assert_eq!(result.metadata.sync_epoch, initial.sync_epoch);
        assert_eq!(
            result
                .metadata
                .change_seq
                .expect("stored watermark")
                .value(),
            0
        );
        assert_eq!(fingerprint(&root.join("secrets")), secrets);
        assert_eq!(fingerprint(&root.join("diagnostics")), diagnostics);
        assert_eq!(fingerprint(&root.join("operator-notes.txt")), notes);
        assert!(!root.join(MIGRATION_FILE).exists());

        let recovery = result.backup.expect("pre-upgrade recovery report");
        assert_eq!(recovery.schema_version, 1);
        assert_eq!(recovery.change_seq, None);
        assert_eq!(recovery.server_id, initial.server_id);
        assert_eq!(recovery.sync_epoch, initial.sync_epoch);
        assert_eq!(
            verify_snapshot(&recovery_path).expect("verify recovery copy"),
            recovery
        );
        let upgraded_path = parent.path().join("after-upgrade");
        let upgraded = create_snapshot(&root, &upgraded_path).expect("snapshot current schema");
        assert_eq!(upgraded.schema_version, SCHEMA_VERSION);
        assert_eq!(
            upgraded
                .change_seq
                .expect("captured stored watermark")
                .value(),
            0
        );

        fs::remove_dir_all(&root).expect("remove original fixture");
        assert_eq!(verify_snapshot(&recovery_path).unwrap(), recovery);
        assert_eq!(verify_snapshot(&upgraded_path).unwrap(), upgraded);
    }

    /// Repeating an upgrade on a clean current root is read-only and does not
    /// inspect, replace, or adopt an existing requested backup destination.
    #[test]
    fn current_schema_is_a_noop_that_preserves_existing_paths() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("current");
        let initial = initialize(&root).expect("initialize current metadata");
        let output = parent.path().join("existing-backup-path");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep.txt"), b"not a migration destination").unwrap();
        let root_before = fingerprint(&root);
        let output_before = fingerprint(&output);

        let result = migrate(&root, &output).expect("already-current no-op");

        assert_eq!(result.metadata, initial);
        assert_eq!(result.from_schema_version, SCHEMA_VERSION);
        assert_eq!(result.to_schema_version, SCHEMA_VERSION);
        assert_eq!(result.backup, None);
        assert!(!result.resumed);
        assert_eq!(fingerprint(&root), root_before);
        assert_eq!(fingerprint(&output), output_before);
    }

    /// Existing or source-nested recovery destinations must not trigger any source
    /// database change or destroy the existing destination.
    #[test]
    fn unsafe_backup_destinations_leave_schema_one_unchanged() {
        let (parent, root, _) = old_fixture();
        let existing = parent.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("keep.txt"), b"existing user bytes").unwrap();
        let source_before = fingerprint(&root);
        let output_before = fingerprint(&existing);
        assert!(migrate(&root, &existing).is_err());
        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&existing), output_before);

        rejected_without_changes(&root, &root.join("snapshots/before-upgrade"));
        assert_eq!(inspect(&root).unwrap().schema_version, 1);
    }

    /// Upgrade takes an exclusive maintenance lock, so both a checked reader and
    /// another writer block it without allowing a snapshot or journal to be created.
    #[test]
    fn shared_and_exclusive_maintenance_holders_block_upgrade() {
        let (parent, root, _) = old_fixture();
        for exclusive in [false, true] {
            let holder = File::open(root.join(LOCK_FILE)).unwrap();
            if exclusive {
                FileExt::try_lock_exclusive(&holder).unwrap();
            } else {
                FileExt::try_lock_shared(&holder).unwrap();
            }
            let output = parent.path().join(format!("blocked-{exclusive}"));
            assert!(matches!(
                rejected_without_changes(&root, &output),
                StorageError::InUse
            ));
            FileExt::unlock(&holder).unwrap();
        }
    }

    /// A recorded but not yet applied upgrade blocks ordinary reads and resumes
    /// with its original verified recovery copy, rather than replacing that copy.
    #[test]
    fn prepared_journal_resumes_and_blocks_ordinary_inspection() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        let backup_before = fingerprint(&output);
        write_journal(&root, &initial, &backup);

        assert!(matches!(
            inspect(&root),
            Err(StorageError::NeedsRecovery(_))
        ));
        let unrelated = parent.path().join("ordinary-snapshot");
        assert!(matches!(
            create_snapshot(&root, &unrelated),
            Err(StorageError::NeedsRecovery(_))
        ));
        assert!(!unrelated.exists());

        let result = migrate(&root, &output).expect("resume prepared upgrade");

        assert!(result.resumed);
        assert_eq!(result.from_schema_version, 1);
        assert_eq!(result.to_schema_version, SCHEMA_VERSION);
        assert_eq!(result.backup, Some(backup));
        assert_eq!(fingerprint(&output), backup_before);
        assert!(!root.join(MIGRATION_FILE).exists());
        assert_eq!(inspect(&root).unwrap().schema_version, SCHEMA_VERSION);
    }

    /// A crash after the new database and marker are ready may leave the journal;
    /// retrying verifies that completed work before removing the journal again.
    #[test]
    fn completed_database_and_marker_with_retained_journal_resume_idempotently() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let first = migrate(&root, &output).unwrap();
        let backup = first.backup.unwrap();
        write_journal(&root, &initial, &backup);
        assert!(inspect(&root).is_err());

        let resumed = migrate(&root, &output).expect("finish retained journal");

        assert!(resumed.resumed);
        assert_eq!(resumed.metadata, first.metadata);
        assert_eq!(resumed.backup, Some(backup.clone()));
        assert!(!root.join(MIGRATION_FILE).exists());
        assert_eq!(verify_snapshot(&output).unwrap(), backup);
        let repeated = migrate(&root, &output).unwrap();
        assert!(!repeated.resumed);
        assert_eq!(repeated.backup, None);
    }

    /// Database commit and marker replacement are separate durable steps. A
    /// recorded upgrade must finish their known mismatch rather than requiring a
    /// manual marker edit or applying the schema change twice.
    #[test]
    fn upgraded_database_with_original_marker_resumes_the_recorded_upgrade() {
        let (parent, root, initial) = old_fixture();
        let original_marker = fs::read(root.join(FORMAT_FILE)).unwrap();
        let output = parent.path().join("recovery-copy");
        let first = migrate(&root, &output).unwrap();
        let backup = first.backup.unwrap();
        fs::write(root.join(FORMAT_FILE), original_marker).unwrap();
        write_journal(&root, &initial, &backup);
        assert!(inspect(&root).is_err());

        let resumed = migrate(&root, &output).expect("finish database/marker mismatch");

        assert!(resumed.resumed);
        assert_eq!(resumed.from_schema_version, 1);
        assert_eq!(resumed.to_schema_version, SCHEMA_VERSION);
        assert_eq!(resumed.metadata, first.metadata);
        assert_eq!(resumed.backup, Some(backup.clone()));
        assert_eq!(verify_snapshot(&output).unwrap(), backup);
        assert_eq!(inspect(&root).unwrap().schema_version, SCHEMA_VERSION);
        assert!(!root.join(MIGRATION_FILE).exists());
    }

    /// A resume request must use the journal's original recovery destination and
    /// must still verify those retained bytes before opening a writer.
    #[test]
    fn resume_refuses_wrong_or_damaged_recovery_copy_without_source_changes() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &initial, &backup);
        let source_before = fingerprint(&root);
        let wrong_output = parent.path().join("different-copy");
        assert!(migrate(&root, &wrong_output).is_err());
        assert!(!wrong_output.exists());
        assert_eq!(fingerprint(&root), source_before);

        let copied_database = output.join(SNAPSHOT_DATABASE_FILE);
        let mut bytes = fs::read(&copied_database).unwrap();
        bytes[100] ^= 1;
        fs::write(&copied_database, bytes).unwrap();
        let damaged_before = fingerprint(&output);
        assert!(migrate(&root, &output).is_err());
        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&output), damaged_before);
    }

    /// A malformed, future, or identity-mismatched journal cannot authorize
    /// mutation, even when the underlying schema-1 source is otherwise valid.
    #[test]
    fn invalid_journals_are_preserved_and_do_not_change_the_source() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &initial, &backup);
        let valid: Value =
            serde_json::from_slice(&fs::read(root.join(MIGRATION_FILE)).unwrap()).unwrap();
        let mut future = valid.clone();
        future["journal_format_version"] = json!(7);
        let mut wrong_identity = valid.clone();
        wrong_identity["server_id"] = json!(jumpstart_domain::ServerId::new());
        for bytes in [
            b"{unfinished journal".to_vec(),
            serde_json::to_vec(&future).unwrap(),
            serde_json::to_vec(&wrong_identity).unwrap(),
        ] {
            fs::write(root.join(MIGRATION_FILE), bytes).unwrap();
            let before = fingerprint(&root);
            assert!(migrate(&root, &output).is_err());
            assert_eq!(fingerprint(&root), before);
        }
    }

    /// A journal symlink cannot redirect recovery decisions to an external file,
    /// even when the target contains a well-formed journal for this installation.
    #[test]
    fn journal_symlink_is_refused_without_touching_its_target() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &initial, &backup);
        let journal_path = root.join(MIGRATION_FILE);
        let target = parent.path().join("outside-journal.json");
        fs::rename(&journal_path, &target).unwrap();
        symlink(&target, &journal_path).unwrap();
        let source_before = fingerprint(&root);
        let target_before = fingerprint(&target);

        assert!(matches!(
            migrate(&root, &output),
            Err(StorageError::InvalidPath { .. })
        ));

        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&target), target_before);
        assert_eq!(fs::read_link(&journal_path).unwrap(), target);
        assert_eq!(verify_snapshot(&output).unwrap(), backup);
    }

    /// Resuming must reject each SQLite sidecar symlink before a connection can
    /// read or write through it, preserving the unrelated destination file.
    #[test]
    fn resume_sidecar_symlinks_are_refused_without_touching_targets() {
        for suffix in ["-wal", "-shm", "-journal"] {
            let (parent, root, initial) = old_fixture();
            let output = parent.path().join("recovery-copy");
            let backup = create_snapshot(&root, &output).unwrap();
            write_journal(&root, &initial, &backup);
            let sidecar = root
                .join(DATABASE_DIRECTORY)
                .join(format!("{DATABASE_FILE}{suffix}"));
            if sidecar.exists() {
                fs::remove_file(&sidecar).unwrap();
            }
            let target = parent.path().join("unrelated-file");
            fs::write(&target, b"preserve unrelated user data").unwrap();
            symlink(&target, &sidecar).unwrap();
            let source_before = fingerprint(&root);
            let target_before = fingerprint(&target);

            assert!(matches!(
                migrate(&root, &output),
                Err(StorageError::InvalidPath { .. })
            ));

            assert_eq!(fingerprint(&root), source_before);
            assert_eq!(fingerprint(&target), target_before);
            assert_eq!(fs::read_link(&sidecar).unwrap(), target);
            assert_eq!(verify_snapshot(&output).unwrap(), backup);
        }
    }

    /// An unknown rollback journal is separate recovery evidence; the migration
    /// journal must not authorize SQLite to consume or remove those bytes.
    #[test]
    fn resume_preserves_a_nonempty_regular_rollback_journal() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &initial, &backup);
        let rollback = root
            .join(DATABASE_DIRECTORY)
            .join(format!("{DATABASE_FILE}-journal"));
        fs::write(&rollback, b"unrecognized rollback recovery evidence").unwrap();
        let source_before = fingerprint(&root);
        let backup_before = fingerprint(&output);

        migrate(&root, &output).expect_err("unexplained rollback state must be refused");

        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&output), backup_before);
    }

    /// Individually valid database metadata must still match its preserved
    /// pre-upgrade snapshot; a changed historical row cannot be silently accepted.
    #[test]
    fn resume_rejects_changed_initial_migration_timestamp() {
        let (parent, root, initial) = old_fixture();
        let output = parent.path().join("recovery-copy");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &initial, &backup);
        let connection = Connection::open(database(&root)).unwrap();
        connection
            .execute(
                "UPDATE schema_migrations SET applied_at = ?1 WHERE version = 1",
                ["2026-10-27T00:00:01.000Z"],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection.close().unwrap();
        let source_before = fingerprint(&root);
        let backup_before = fingerprint(&output);

        migrate(&root, &output).expect_err("changed preserved history must be refused");

        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&output), backup_before);
    }

    /// A prepared journal must not cause writable opening of an unrelated SQLite
    /// application or future schema, even when its path and marker look familiar.
    #[test]
    fn resume_rejects_foreign_sqlite_headers_without_changing_database_bytes() {
        for (pragma, value) in [
            ("application_id", 0x1234_5678_i64),
            ("user_version", i64::from(SCHEMA_VERSION) + 1),
        ] {
            let (parent, root, initial) = old_fixture();
            let output = parent.path().join("recovery-copy");
            let backup = create_snapshot(&root, &output).unwrap();
            write_journal(&root, &initial, &backup);
            let connection = Connection::open(database(&root)).unwrap();
            connection.pragma_update(None, pragma, value).unwrap();
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            connection.close().unwrap();
            let database_before = fs::read(database(&root)).unwrap();
            let marker_before = fs::read(root.join(FORMAT_FILE)).unwrap();
            let journal_before = fs::read(root.join(MIGRATION_FILE)).unwrap();

            migrate(&root, &output).expect_err("unsupported SQLite header must be refused");

            assert_eq!(fs::read(database(&root)).unwrap(), database_before);
            assert_eq!(fs::read(root.join(FORMAT_FILE)).unwrap(), marker_before);
            assert_eq!(fs::read(root.join(MIGRATION_FILE)).unwrap(), journal_before);
            assert_eq!(verify_snapshot(&output).unwrap(), backup);
        }
    }

    /// Missing, corrupt, or unsupported sources are never initialized or repaired
    /// by migration, and cannot produce a misleading recovery artifact.
    #[test]
    fn invalid_sources_do_not_initialize_repair_or_publish() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        let output = parent.path().join("missing-source-backup");
        assert!(migrate(&missing, &output).is_err());
        assert!(!missing.exists());
        assert!(!output.exists());

        let (_guard, root, _) = old_fixture();
        fs::write(database(&root), b"damaged SQLite bytes").unwrap();
        rejected_without_changes(&root, &parent.path().join("corrupt-source-backup"));

        let (_guard, root, _) = old_fixture();
        let marker_path = root.join(FORMAT_FILE);
        let mut marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker["schema_version"] = json!(SCHEMA_VERSION + 1);
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(matches!(
            rejected_without_changes(&root, &parent.path().join("future-source-backup")),
            StorageError::UnsupportedSchema { .. }
        ));

        let (_guard, root, _) = old_fixture();
        let connection = Connection::open(database(&root)).unwrap();
        connection
            .execute_batch("UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        connection.close().unwrap();
        assert!(matches!(
            rejected_without_changes(&root, &parent.path().join("invalid-checksum-backup")),
            StorageError::MigrationMismatch
        ));
    }

    /// Metadata roots and critical files must remain actual entries, so migration
    /// cannot be redirected through final symlinks into unrelated user data.
    #[test]
    fn root_and_critical_file_symlinks_are_refused_without_touching_targets() {
        let (parent, root, _) = old_fixture();
        let alias = parent.path().join("root-alias");
        symlink(&root, &alias).unwrap();
        let before = fingerprint(&root);
        assert!(migrate(&alias, &parent.path().join("alias-backup")).is_err());
        assert_eq!(fingerprint(&root), before);

        for relative in [FORMAT_FILE, LOCK_FILE, "database/jumpstart.sqlite"] {
            let (parent, root, _) = old_fixture();
            let original = root.join(relative);
            let target = parent.path().join("outside-original");
            fs::rename(&original, &target).unwrap();
            symlink(&target, &original).unwrap();
            let target_before = fingerprint(&target);
            rejected_without_changes(&root, &parent.path().join("symlink-backup"));
            assert_eq!(fingerprint(&target), target_before);
        }
    }
    /// Preserve an older stored position as the new ledger's floor, even at its limit.
    #[test]
    fn schema2_upgrade_preserves_the_existing_change_floor() {
        for sequence in [42_i64, i64::MAX] {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("schema-two");
            let before = super::support::create_schema2(&root);
            let connection =
                Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            connection
                .execute("UPDATE server_state SET change_seq=?1", [sequence])
                .unwrap();
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            connection.close().unwrap();
            let backup_path = parent.path().join("before-catalog");
            let result = migrate(&root, &backup_path).unwrap();
            assert_eq!(result.from_schema_version, 2);
            assert_eq!(result.to_schema_version, SCHEMA_VERSION);
            assert_eq!(result.metadata.server_id, before.server_id);
            assert_eq!(result.metadata.sync_epoch, before.sync_epoch);
            assert_eq!(result.metadata.change_seq.unwrap().value(), sequence as u64);
            let database =
                Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            let floor: i64 = database
                .query_row("SELECT change_floor FROM server_state", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(floor, sequence);
            database.close().unwrap();
            let after_path = parent.path().join("catalog-snapshot");
            let after = create_snapshot(&root, &after_path).unwrap();
            assert_eq!(after.scope, jumpstart_storage::SnapshotScope::Metadata);
            fs::remove_dir_all(&root).unwrap();
            assert_eq!(verify_snapshot(&after_path).unwrap(), after);
            let old = verify_snapshot(&backup_path).unwrap();
            assert_eq!(old.schema_version, 2);
            assert_eq!(old.change_seq.unwrap().value(), sequence as u64);
        }
    }

    /// Finish a journal emitted by the previous binary at its original schema-2 target.
    #[test]
    fn legacy_journal_finishes_two_before_a_separate_upgrade_to_current() {
        let (parent, root, before) = old_fixture();
        let first_output = parent.path().join("legacy-backup");
        let backup = create_snapshot(&root, &first_output).unwrap();
        write_journal(&root, &before, &backup);
        let journal_path = root.join(MIGRATION_FILE);
        let mut journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        journal["journal_format_version"] = 1.into();
        journal["to_schema_version"] = 2.into();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let legacy = migrate(&root, &first_output).unwrap();
        assert!(legacy.resumed);
        assert_eq!(legacy.to_schema_version, 2);
        assert_eq!(legacy.metadata.schema_version, 2);
        assert_eq!(legacy.backup, Some(backup));
        assert!(!journal_path.exists());
        let final_result = migrate(&root, &parent.path().join("next-backup")).unwrap();
        assert_eq!(final_result.from_schema_version, 2);
        assert_eq!(final_result.to_schema_version, SCHEMA_VERSION);
        assert_eq!(final_result.metadata.server_id, before.server_id);
        assert_eq!(final_result.metadata.sync_epoch, before.sync_epoch);
    }

    /// Complete an old job-ledger journal, then preserve real jobs when adding the catalog.
    #[test]
    fn catalog_upgrade_preserves_completed_jobs_and_legacy_snapshot_results() {
        let (parent, root, initial) = old_fixture();
        let legacy_output = parent.path().join("before-ledger");
        let legacy_backup = create_snapshot(&root, &legacy_output).unwrap();
        write_journal(&root, &initial, &legacy_backup);
        let journal_path = root.join(MIGRATION_FILE);
        let mut journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        journal["journal_format_version"] = 2.into();
        journal["to_schema_version"] = 3.into();
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let legacy = migrate(&root, &legacy_output).unwrap();
        assert_eq!(legacy.to_schema_version, 3);
        assert!(legacy.resumed);

        let old_job_output = parent.path().join("old-job-snapshot");
        let job = jumpstart_storage::enqueue_snapshot_job(&root, &old_job_output).unwrap();
        let completed = jumpstart_storage::run_snapshot_job(&root, job.job_id).unwrap();
        let history = jumpstart_storage::job_history(&root, job.job_id).unwrap();
        let before = inspect(&root).unwrap();
        let backup_path = parent.path().join("before-catalog");
        let upgraded = migrate(&root, &backup_path).unwrap();

        assert_eq!(upgraded.from_schema_version, 3);
        assert_eq!(upgraded.to_schema_version, SCHEMA_VERSION);
        assert_eq!(upgraded.metadata.server_id, before.server_id);
        assert_eq!(upgraded.metadata.sync_epoch, before.sync_epoch);
        assert_eq!(upgraded.metadata.change_seq, before.change_seq);
        assert_eq!(
            verify_snapshot(&backup_path).unwrap(),
            upgraded.backup.unwrap()
        );
        assert_eq!(
            verify_snapshot(&old_job_output).unwrap(),
            completed.result.clone().unwrap()
        );
        assert_eq!(
            jumpstart_storage::run_snapshot_job(&root, job.job_id).unwrap(),
            completed
        );
        assert_eq!(
            jumpstart_storage::job_history(&root, job.job_id).unwrap(),
            history
        );
        let new_job_output = parent.path().join("new-job-snapshot");
        let next = jumpstart_storage::enqueue_snapshot_job(&root, &new_job_output).unwrap();
        let next = jumpstart_storage::run_snapshot_job(&root, next.job_id).unwrap();
        assert_eq!(next.result.unwrap().schema_version, SCHEMA_VERSION);
    }

    /// Validate the complete schema-2 migration prefix before resuming a new upgrade.
    #[test]
    fn schema2_resume_rejects_a_changed_second_migration_record() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("schema-two");
        let before = super::support::create_schema2(&root);
        let output = parent.path().join("before-catalog");
        let backup = create_snapshot(&root, &output).unwrap();
        write_journal(&root, &before, &backup);
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        connection
            .execute(
                "UPDATE schema_migrations SET applied_at='changed' WHERE version=2",
                [],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection.close().unwrap();
        let source_before = fingerprint(&root);
        let backup_before = fingerprint(&output);
        assert!(migrate(&root, &output).is_err());
        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&output), backup_before);
    }
}

/// Platforms without the storage implementation explicitly refuse migration
/// without creating either the requested metadata root or recovery destination.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_migration_performs_no_filesystem_work() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("missing-metadata");
    let output = parent.path().join("missing-backup");
    assert!(matches!(
        jumpstart_storage::migrate(&root, &output),
        Err(jumpstart_storage::StorageError::UnsupportedPlatform)
    ));
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}
