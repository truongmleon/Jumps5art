//! Upgrades existing bookkeeping without losing the state needed after interruption.
//!
//! A database transaction cannot also replace format.json atomically. This module
//! first preserves a verified snapshot, then publishes a journal before changing
//! SQLite. A later explicit upgrade can use that journal to finish the database,
//! checkpoint, and marker steps. Ordinary inspection never repairs these files.
//! Schema 2 adds the stored change position needed by future mutations; it does
//! not add a job store, change-feed service, daemon, or full-server recovery.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use jumpstart_domain::{ServerId, SyncEpoch};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    VerifiedMetadata, apply_schema2, destination, io, lock_metadata, open_directory, open_regular,
    read_change_seq, read_marker, readonly_uri, snapshot, sync_directory, verify_database,
    verify_locked_metadata,
};
use crate::{
    DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, FORMAT_VERSION, FormatMarker, MIGRATION_FILE,
    MigrationReport, Result, SCHEMA_VERSION, SnapshotReport, StorageError,
};

const JOURNAL_LIMIT: u64 = 64 * 1024;

/// Keeps the recovery evidence that must survive between the database and marker.
///
/// The old snapshot identifies the preserved source independently of the root.
/// The identity and version fields restrict recovery to this one known upgrade;
/// an arbitrary journal must not authorize opening a different database for writes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct MigrationJournal {
    journal_format_version: u32,
    from_schema_version: u32,
    to_schema_version: u32,
    server_id: ServerId,
    sync_epoch: SyncEpoch,
    backup: SnapshotReport,
}

/// Names interruption boundaries so tests can stop the actual workflow precisely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationStep {
    BackupPrepared,
    JournalPublished,
    DatabaseCommitted,
    Checkpointed,
    MarkerPublished,
    JournalRemoved,
}

/// Run the supported upgrade, retaining exclusive access until all files agree.
///
/// `root` and `snapshot_output` are borrowed `&Path` values for existing metadata
/// and its new recovery snapshot. Returns `Result<MigrationReport>`; a clean
/// current root is a no-op. Errors retain published recovery evidence for retry.
pub(crate) fn migrate(root: &Path, snapshot_output: &Path) -> Result<MigrationReport> {
    migrate_with_hook(root, snapshot_output, |_| Ok(()))
}

/// Execute the same workflow with an internal milestone observer for fault tests.
///
/// `root` and `snapshot_output` select the paths. `milestone` receives each
/// `MigrationStep` and may return an error to simulate interruption. Returns
/// `Result<MigrationReport>` only after verification and durable journal removal.
fn migrate_with_hook(
    root: &Path,
    snapshot_output: &Path,
    mut milestone: impl FnMut(MigrationStep) -> Result<()>,
) -> Result<MigrationReport> {
    let (root, lock) = lock_metadata(root, true)?;
    let journal_exists = match fs::symlink_metadata(root.join(MIGRATION_FILE)) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io("inspect migration journal", &root, error)),
    };

    let journal = if journal_exists {
        let journal = read_journal(&root)?;
        validate_journal(&root, snapshot_output, &journal)?;
        journal
    } else {
        let source = verify_locked_metadata(root.clone(), lock)?;
        if source.marker.schema_version == SCHEMA_VERSION {
            return Ok(MigrationReport {
                metadata: source.report.clone(),
                from_schema_version: SCHEMA_VERSION,
                to_schema_version: SCHEMA_VERSION,
                backup: None,
                resumed: false,
            });
        }
        let backup = snapshot::create_snapshot_from_verified(&source, snapshot_output)?;
        milestone(MigrationStep::BackupPrepared)?;
        let journal = MigrationJournal {
            journal_format_version: 1,
            from_schema_version: 1,
            to_schema_version: 2,
            server_id: source.marker.server_id,
            sync_epoch: source.marker.sync_epoch,
            backup,
        };
        // Keep the same exclusive file handle; acquiring another lock here would
        // leave a gap or conflict with our own still-open lock handle.
        let VerifiedMetadata {
            connection, _lock, ..
        } = source;
        connection
            .close()
            .map_err(|(_, error)| StorageError::Database(error))?;
        return finish_new_upgrade(&root, _lock, journal, &mut milestone);
    };

    finish_upgrade(&root, lock, journal, true, &mut milestone)
}

/// Publish recovery evidence before the first writable database connection opens.
///
/// `root` is the checked path, `lock` is the continuously held exclusive handle,
/// `journal` describes the verified backup, and `milestone` observes fault points.
/// Returns `Result<MigrationReport>` through the shared recovery workflow.
fn finish_new_upgrade(
    root: &Path,
    lock: std::fs::File,
    journal: MigrationJournal,
    milestone: &mut impl FnMut(MigrationStep) -> Result<()>,
) -> Result<MigrationReport> {
    publish_json(root, MIGRATION_FILE, &journal, false)?;
    milestone(MigrationStep::JournalPublished)?;
    finish_upgrade(root, lock, journal, false, milestone)
}

/// Complete a prepared or interrupted upgrade according to checked database state.
///
/// `root` is the canonical metadata root; `lock` retains exclusive access;
/// `journal` identifies the preserved old state; `resumed` records whether it came
/// from a previous call; `milestone` enables precise fault tests. Returns
/// `Result<MigrationReport>` or a failure without deleting the recovery snapshot.
fn finish_upgrade(
    root: &Path,
    lock: std::fs::File,
    journal: MigrationJournal,
    resumed: bool,
    milestone: &mut impl FnMut(MigrationStep) -> Result<()>,
) -> Result<MigrationReport> {
    validate_journal(root, &journal.backup.root, &journal)?;
    let marker = read_marker(root)?;
    if marker.server_id != journal.server_id || marker.sync_epoch != journal.sync_epoch {
        return Err(StorageError::IdentityMismatch);
    }
    // A previous rename may have succeeded while its directory sync failed.
    // Re-establish durable recovery evidence before opening a database writer.
    open_regular(&root.join(MIGRATION_FILE))?
        .sync_all()
        .map_err(|error| io("synchronize retained migration journal", root, error))?;
    sync_directory(root)?;
    let database_path = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
    let mut database = open_regular(&database_path)?;
    let mut header = [0_u8; 100];
    database
        .read_exact(&mut header)
        .map_err(|error| io("read migration database header", &database_path, error))?;
    if &header[..16] != b"SQLite format 3\0" || header[18] != 2 || header[19] != 2 {
        return Err(StorageError::InvalidMigration(
            "source must retain WAL format",
        ));
    }
    let live_wal = check_sidecars(root)?;
    let read_flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let reader = if live_wal {
        // Immutable mode would ignore committed WAL and see the old schema.
        Connection::open_with_flags(&database_path, read_flags)?
    } else {
        Connection::open_with_flags(
            readonly_uri(&database_path),
            read_flags | OpenFlags::SQLITE_OPEN_URI,
        )?
    };
    reader.busy_timeout(Duration::from_secs(5))?;
    reader.pragma_update(None, "trusted_schema", false)?;
    reader.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    validate_database_state(&reader, &marker, &journal.backup)?;
    reader
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    let mut connection = Connection::open_with_flags(
        &database_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    if !connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)? {
        return Err(StorageError::InvalidMigration(
            "close-time checkpoints remain enabled",
        ));
    }
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode != "wal" {
        return Err(StorageError::InvalidMigration(
            "recovery writer did not retain WAL mode",
        ));
    }
    let mut database_marker = validate_database_state(&connection, &marker, &journal.backup)?;
    if database_marker.schema_version == 1 {
        apply_schema2(&mut connection)?;
    }
    milestone(MigrationStep::DatabaseCommitted)?;
    database_marker.schema_version = 2;
    verify_database(&connection, &database_marker)?;
    if read_change_seq(&connection, 2)?.map(|value| value.value()) != Some(0) {
        return Err(StorageError::InvalidMigration(
            "interrupted bootstrap upgrade has an unexpected change position",
        ));
    }
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 {
        return Err(StorageError::NeedsRecovery("migration checkpoint is busy"));
    }
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    database
        .sync_all()
        .map_err(|error| io("synchronize migrated database", &database_path, error))?;
    sync_directory(&root.join(DATABASE_DIRECTORY))?;
    milestone(MigrationStep::Checkpointed)?;

    // Replacing the marker happens only after the new database is checked and
    // checkpointed. A crash before this rename leaves the journal for recovery.
    publish_json(root, FORMAT_FILE, &database_marker, true)?;
    milestone(MigrationStep::MarkerPublished)?;
    verify_published_root(root, &database_marker)?;
    if read_journal(root)? != journal {
        return Err(StorageError::InvalidMigration("migration journal changed"));
    }
    fs::remove_file(root.join(MIGRATION_FILE))
        .map_err(|error| io("remove completed migration journal", root, error))?;
    sync_directory(root)?;
    milestone(MigrationStep::JournalRemoved)?;
    let verified = verify_locked_metadata(root.to_path_buf(), lock)?;
    Ok(MigrationReport {
        metadata: verified.report,
        from_schema_version: 1,
        to_schema_version: 2,
        backup: Some(journal.backup),
        resumed,
    })
}

/// Read back the checkpointed database and marker before discarding their journal.
///
/// `root` is exclusively locked and `expected` identifies the completed upgrade.
/// Returns `Result<()>` only when the on-disk marker and immutable database agree;
/// a failed read-back leaves the recovery journal available for explicit retry.
fn verify_published_root(root: &Path, expected: &FormatMarker) -> Result<()> {
    let actual = read_marker(root)?;
    if actual.format_version != expected.format_version
        || actual.schema_version != expected.schema_version
        || actual.server_id != expected.server_id
        || actual.sync_epoch != expected.sync_epoch
    {
        return Err(StorageError::InvalidMigration(
            "published format marker differs",
        ));
    }
    let reader = Connection::open_with_flags(
        readonly_uri(&root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    reader.pragma_update(None, "trusted_schema", false)?;
    verify_database(&reader, expected)?;
    reader
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(())
}

/// Validate a WAL-aware database view before opening or using its recovery writer.
///
/// `connection` is a read-only or write-capable view, `marker` is the checked root
/// marker, and `backup` is the verified schema-1 artifact. Returns
/// `Result<FormatMarker>` with the actual schema. Only DB1/marker1, DB2/marker1,
/// and DB2/marker2 are recognized interrupted states; other combinations fail.
fn validate_database_state(
    connection: &Connection,
    marker: &FormatMarker,
    backup: &SnapshotReport,
) -> Result<FormatMarker> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !matches!(version, 1 | 2) || (version == 1 && marker.schema_version != 1) {
        return Err(StorageError::InvalidMigration(
            "database and marker are not a recognized interrupted upgrade",
        ));
    }
    let mut expected = marker.clone();
    expected.schema_version = version;
    verify_database(connection, &expected)?;
    compare_preserved_state(connection, backup)?;
    if version == 2 && read_change_seq(connection, 2)?.map(|value| value.value()) != Some(0) {
        return Err(StorageError::InvalidMigration(
            "interrupted bootstrap upgrade has an unexpected change position",
        ));
    }
    Ok(expected)
}

/// Check that bootstrap migration data still matches the preserved snapshot.
///
/// `connection` is the checked current SQLite database; `backup` is its verified
/// schema-1 recovery artifact. Returns `Result<()>` after comparing the initial
/// migration record; identity is checked separately by database/journal validation.
/// This rejects changed migration data even when schema and identity still agree.
fn compare_preserved_state(connection: &Connection, backup: &SnapshotReport) -> Result<()> {
    let saved = Connection::open_with_flags(
        readonly_uri(&backup.root.join(crate::SNAPSHOT_DATABASE_FILE)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let initial_record = |database: &Connection| -> rusqlite::Result<(String, String, String)> {
        database.query_row(
            "SELECT name, checksum, applied_at FROM schema_migrations WHERE version=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
    };
    if initial_record(connection)? != initial_record(&saved)? {
        return Err(StorageError::InvalidMigration(
            "initial migration record differs from the preserved snapshot",
        ));
    }
    saved
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(())
}

/// Read a strict, bounded journal without following its final symlink.
///
/// `root` is the metadata directory. Returns `Result<MigrationJournal>` after
/// reading at most 64 KiB plus one detection byte; invalid files/JSON fail.
fn read_journal(root: &Path) -> Result<MigrationJournal> {
    let path = root.join(MIGRATION_FILE);
    let mut bytes = Vec::new();
    open_regular(&path)?
        .take(JOURNAL_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io("read migration journal", &path, error))?;
    if bytes.len() as u64 > JOURNAL_LIMIT {
        return Err(StorageError::InvalidMigration("journal exceeds 64 KiB"));
    }
    serde_json::from_slice(&bytes).map_err(StorageError::InvalidMigrationJournal)
}

/// Bind a journal to this root, upgrade, and independently verified recovery copy.
///
/// `root` is the canonical metadata directory; `output` is the caller's requested
/// backup path; `journal` is untrusted decoded data. Returns `Result<()>` only for
/// the supported 1-to-2 upgrade and matching old identity, path, and artifact.
fn validate_journal(root: &Path, output: &Path, journal: &MigrationJournal) -> Result<()> {
    if journal.journal_format_version != 1
        || journal.from_schema_version != 1
        || journal.to_schema_version != 2
        || journal.backup.schema_version != 1
        || journal.backup.format_version != FORMAT_VERSION
        || journal.backup.server_id != journal.server_id
        || journal.backup.sync_epoch != journal.sync_epoch
    {
        return Err(StorageError::InvalidMigration(
            "unsupported or inconsistent journal",
        ));
    }
    let (parent, name) = destination(output)?;
    let requested = parent.join(name);
    if requested != journal.backup.root || requested.starts_with(root) {
        return Err(StorageError::InvalidMigration(
            "resume requires the journal's original snapshot path outside metadata",
        ));
    }
    if snapshot::verify_snapshot(&requested)? != journal.backup {
        return Err(StorageError::InvalidMigration(
            "recovery snapshot no longer matches",
        ));
    }
    Ok(())
}

/// Refuse symlink and special-file SQLite sidecars before opening a recovery writer.
///
/// `root` is the metadata path. Returns `Result<bool>` indicating a live WAL.
/// Refuses nonregular sidecars and every nonempty rollback journal, which this
/// WAL-mode workflow never creates. It never deletes journal bytes itself.
fn check_sidecars(root: &Path) -> Result<bool> {
    let mut live_wal = false;
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = root
            .join(DATABASE_DIRECTORY)
            .join(format!("{DATABASE_FILE}{suffix}"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                open_regular(&path)?;
                if suffix == "-journal" && metadata.len() > 0 {
                    return Err(StorageError::InvalidMigration(
                        "unexpected rollback journal is outside this migration workflow",
                    ));
                }
                if suffix == "-wal" && metadata.len() > 0 {
                    live_wal = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("inspect migration sidecar", &path, error)),
        }
    }
    Ok(live_wal)
}

/// Publish fully flushed JSON through a same-directory atomic rename.
///
/// `root` is the checked directory, `name` is an internal fixed filename, `value`
/// supplies serializable data, and `replace` permits replacing only the existing
/// format marker. Returns `Result<()>` after file and directory synchronization.
/// Errors retain private temporary files; an error after rename may leave `name`.
fn publish_json(root: &Path, name: &str, value: &impl Serialize, replace: bool) -> Result<()> {
    let temporary_name = format!(".{name}-{}", Uuid::new_v4());
    let temporary = root.join(&temporary_name);
    let bytes = serde_json::to_vec_pretty(value).map_err(StorageError::InvalidMigrationJournal)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| io("create migration JSON staging", &temporary, error))?;
    file.write_all(&bytes)
        .map_err(|error| io("write migration JSON staging", &temporary, error))?;
    file.sync_all()
        .map_err(|error| io("synchronize migration JSON staging", &temporary, error))?;
    let directory = open_directory(root)?;
    if replace {
        open_regular(&root.join(name))?;
        rustix::fs::renameat(&directory, temporary_name.as_str(), &directory, name)
            .map_err(|error| io("replace migrated format marker", root, error.into()))?;
    } else {
        rustix::fs::renameat_with(
            &directory,
            temporary_name.as_str(),
            &directory,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            io(
                "publish migration journal without replacement",
                root,
                error.into(),
            )
        })?;
    }
    directory
        .sync_all()
        .map_err(|error| io("synchronize migration directory", root, error))
}

#[cfg(test)]
#[path = "../../tests/support/mod.rs"]
mod schema1_fixture;

#[cfg(test)]
mod tests {
    use super::*;

    /// Check every cross-file interruption boundary and idempotent explicit retry.
    #[test]
    fn interruptions_resume_without_reapplying_the_schema_change() {
        for stopped in [
            MigrationStep::JournalPublished,
            MigrationStep::DatabaseCommitted,
            MigrationStep::Checkpointed,
            MigrationStep::MarkerPublished,
            MigrationStep::JournalRemoved,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("metadata");
            let backup = temporary.path().join("before-upgrade");
            let before = schema1_fixture::create_schema1(&root);
            assert!(
                migrate_with_hook(&root, &backup, |step| {
                    if step == stopped {
                        Err(StorageError::NeedsRecovery("injected interruption"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            if stopped != MigrationStep::JournalRemoved {
                assert!(matches!(
                    crate::inspect(&root),
                    Err(StorageError::NeedsRecovery(_))
                ));
            }
            let resumed = migrate(&root, &backup).unwrap();
            assert_eq!(resumed.metadata.schema_version, 2);
            assert_eq!(resumed.metadata.server_id, before.server_id);
            assert_eq!(resumed.metadata.sync_epoch, before.sync_epoch);
            assert!(!root.join(MIGRATION_FILE).exists());
            assert_eq!(
                snapshot::verify_snapshot(&backup).unwrap().schema_version,
                1
            );
        }
    }

    /// Exit without destructors after SQL commit so the parent sees a real live WAL.
    #[test]
    fn crash_child() {
        let Some(root) = std::env::var_os("JUMPSTART_MIGRATION_CRASH_ROOT") else {
            return;
        };
        let backup = std::env::var_os("JUMPSTART_MIGRATION_CRASH_BACKUP").unwrap();
        migrate_with_hook(Path::new(&root), Path::new(&backup), |step| {
            if step == MigrationStep::DatabaseCommitted {
                std::process::exit(93);
            }
            Ok(())
        })
        .unwrap();
        panic!("crash boundary was not reached");
    }

    /// Recover committed WAL after process exit while the old marker is still present.
    #[test]
    fn process_exit_after_commit_is_recoverable() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("metadata");
        let backup = temporary.path().join("before-upgrade");
        let before = schema1_fixture::create_schema1(&root);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::migration::tests::crash_child",
                "--nocapture",
            ])
            .env("JUMPSTART_MIGRATION_CRASH_ROOT", &root)
            .env("JUMPSTART_MIGRATION_CRASH_BACKUP", &backup)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(93));
        assert!(
            fs::metadata(
                root.join(DATABASE_DIRECTORY)
                    .join(format!("{DATABASE_FILE}-wal"))
            )
            .unwrap()
            .len()
                > 0
        );
        assert!(matches!(
            crate::inspect(&root),
            Err(StorageError::NeedsRecovery(_))
        ));
        let resumed = migrate(&root, &backup).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.metadata.server_id, before.server_id);
        assert_eq!(resumed.metadata.sync_epoch, before.sync_epoch);
        assert_eq!(resumed.metadata.change_seq.unwrap().value(), 0);
    }
}
