//! Upgrades existing bookkeeping without losing the state needed after interruption.
//!
//! A database transaction cannot also replace format.json atomically. This module
//! first preserves a verified snapshot, then publishes a journal before changing
//! SQLite. A later explicit upgrade can use that journal to finish the database,
//! checkpoint, and marker steps. Ordinary inspection never repairs these files.
//! Schema 3 adds durable job/change records while preserving schema-1/2 recovery.
//! A legacy journal still finishes its original target before a new upgrade begins.

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
    VerifiedMetadata, apply_schema2, apply_schema3, destination, io, lock_metadata, open_directory,
    open_regular, read_change_seq, read_marker, readonly_uri, schema4, schema5, snapshot,
    sync_directory, verify_database, verify_locked_metadata,
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
    IntermediateCommitted,
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
            journal_format_version: 4,
            from_schema_version: source.marker.schema_version,
            to_schema_version: SCHEMA_VERSION,
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
    validate_database_state(&reader, &marker, &journal)?;
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
    let mut database_marker = validate_database_state(&connection, &marker, &journal)?;
    if database_marker.schema_version == 1 {
        apply_schema2(&mut connection)?;
        database_marker.schema_version = 2;
        if journal.to_schema_version > 2 {
            milestone(MigrationStep::IntermediateCommitted)?;
        }
    }
    if database_marker.schema_version == 2 && journal.to_schema_version >= 3 {
        apply_schema3(&mut connection)?;
        database_marker.schema_version = 3;
        if journal.to_schema_version > 3 {
            milestone(MigrationStep::IntermediateCommitted)?;
        }
    }
    if database_marker.schema_version == 3 && journal.to_schema_version >= 4 {
        schema4::apply(&mut connection)?;
        database_marker.schema_version = 4;
        if journal.to_schema_version > 4 {
            milestone(MigrationStep::IntermediateCommitted)?;
        }
    }
    if database_marker.schema_version == 4 && journal.to_schema_version == 5 {
        schema5::apply(&mut connection)?;
    }
    milestone(MigrationStep::DatabaseCommitted)?;
    let database_marker = validate_database_state(&connection, &marker, &journal)?;
    if database_marker.schema_version != journal.to_schema_version {
        return Err(StorageError::InvalidMigration(
            "migration did not reach the journal's target schema",
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
        from_schema_version: journal.from_schema_version,
        to_schema_version: journal.to_schema_version,
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

/// Validate a WAL-aware database against one explicitly supported upgrade journal.
///
/// `connection` is the current view, `marker` is the checked root marker, and
/// `journal` binds its preserved baseline and target. Returns `Result<FormatMarker>`
/// for recognized forward stages only. A target marker cannot precede its database.
fn validate_database_state(
    connection: &Connection,
    marker: &FormatMarker,
    journal: &MigrationJournal,
) -> Result<FormatMarker> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !(1..=SCHEMA_VERSION).contains(&version)
        || version < journal.from_schema_version
        || version > journal.to_schema_version
        || ![journal.from_schema_version, journal.to_schema_version]
            .contains(&marker.schema_version)
        || (marker.schema_version == journal.to_schema_version
            && version != journal.to_schema_version)
    {
        return Err(StorageError::InvalidMigration(
            "database and marker are not a recognized interrupted upgrade",
        ));
    }
    let mut expected = marker.clone();
    expected.schema_version = version;
    verify_database(connection, &expected)?;
    compare_preserved_state(connection, &journal.backup)?;
    let baseline = journal.backup.change_seq.map_or(0, |value| value.value());
    if version >= 2
        && read_change_seq(connection, version)?.map(|value| value.value()) != Some(baseline)
    {
        return Err(StorageError::InvalidMigration(
            "interrupted upgrade has a changed position",
        ));
    }
    if version >= 3 && journal.from_schema_version < 3 {
        let (floor, jobs): (i64, i64) = connection.query_row(
            "SELECT change_floor, (SELECT count(*) FROM jobs) FROM server_state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if u64::try_from(floor).ok() != Some(baseline) || jobs != 0 {
            return Err(StorageError::InvalidMigration(
                "interrupted upgrade contains new job work",
            ));
        }
    }
    Ok(expected)
}

/// Check that old migration records and the existing job ledger match the snapshot.
///
/// `connection` is the checked database; `backup` identifies its verified baseline.
/// Returns `Result<()>` after comparing the complete original migration prefix,
/// including its timestamps. For a schema-3 baseline, it also compares every
/// saved job and change in order, so resume cannot silently adopt altered work.
fn compare_preserved_state(connection: &Connection, backup: &SnapshotReport) -> Result<()> {
    let saved = Connection::open_with_flags(
        readonly_uri(&backup.root.join(crate::SNAPSHOT_DATABASE_FILE)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let original_records =
        |database: &Connection| -> rusqlite::Result<Vec<(i64, String, String, String)>> {
            database.prepare("SELECT version, name, checksum, applied_at FROM schema_migrations WHERE version<=?1 ORDER BY version")?
            .query_map([backup.schema_version], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect()
        };
    if original_records(connection)? != original_records(&saved)? {
        return Err(StorageError::InvalidMigration(
            "migration history differs from the preserved snapshot",
        ));
    }
    if backup.schema_version >= 3 {
        for query in [
            "SELECT * FROM server_state ORDER BY singleton",
            "SELECT * FROM jobs ORDER BY job_id",
            "SELECT * FROM change_batches ORDER BY seq",
        ] {
            if !same_rows(connection, &saved, query)? {
                return Err(StorageError::InvalidMigration(
                    "job ledger differs from the preserved snapshot",
                ));
            }
        }
    }
    saved
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(())
}

/// Compare one ordered projection in the current and saved databases.
///
/// `query` is a fixed internal SELECT with a stable ordering. Returns whether
/// the rows and their SQLite values agree. Reads one row from each connection
/// at a time so a large job history does not become a second in-memory copy.
fn same_rows(current: &Connection, saved: &Connection, query: &str) -> Result<bool> {
    let mut current_statement = current.prepare(query)?;
    let mut saved_statement = saved.prepare(query)?;
    let columns = current_statement.column_count();
    if columns != saved_statement.column_count() {
        return Ok(false);
    }
    let mut current_rows = current_statement.query([])?;
    let mut saved_rows = saved_statement.query([])?;
    loop {
        match (current_rows.next()?, saved_rows.next()?) {
            (Some(current_row), Some(saved_row)) => {
                for column in 0..columns {
                    if current_row.get_ref(column)? != saved_row.get_ref(column)? {
                        return Ok(false);
                    }
                }
            }
            (None, None) => return Ok(true),
            _ => return Ok(false),
        }
    }
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
/// a supported recorded upgrade and matching old identity, path, and artifact.
fn validate_journal(root: &Path, output: &Path, journal: &MigrationJournal) -> Result<()> {
    let supported = (journal.journal_format_version == 1
        && journal.from_schema_version == 1
        && journal.to_schema_version == 2)
        || (journal.journal_format_version == 2
            && matches!(journal.from_schema_version, 1 | 2)
            && journal.to_schema_version == 3)
        || (journal.journal_format_version == 3
            && matches!(journal.from_schema_version, 1..=3)
            && journal.to_schema_version == 4)
        || (journal.journal_format_version == 4
            && matches!(journal.from_schema_version, 1..=4)
            && journal.to_schema_version == 5);
    if !supported
        || journal.backup.schema_version != journal.from_schema_version
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
        let _isolation = crate::isolate_process_tests();
        for stopped in [
            MigrationStep::JournalPublished,
            MigrationStep::IntermediateCommitted,
            MigrationStep::DatabaseCommitted,
            MigrationStep::Checkpointed,
            MigrationStep::MarkerPublished,
            MigrationStep::JournalRemoved,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("metadata");
            let backup = temporary.path().join("before-upgrade");
            let before = schema1_fixture::create_schema1(&root);
            let mut reached = false;
            let outcome = migrate_with_hook(&root, &backup, |step| {
                if step == stopped {
                    reached = true;
                    Err(StorageError::NeedsRecovery("injected interruption"))
                } else {
                    Ok(())
                }
            });
            assert!(reached, "{stopped:?} was not reached: {outcome:?}");
            assert!(
                matches!(&outcome, Err(StorageError::NeedsRecovery(reason)) if *reason == "injected interruption"),
                "unexpected result at {stopped:?}: {outcome:?}"
            );
            if stopped != MigrationStep::JournalRemoved {
                let inspection = crate::inspect(&root);
                assert!(
                    matches!(&inspection, Err(StorageError::NeedsRecovery(_))),
                    "inspection after {stopped:?}: {inspection:?}"
                );
            }
            let resumed = migrate(&root, &backup).unwrap();
            assert_eq!(resumed.metadata.schema_version, SCHEMA_VERSION);
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
        let _isolation = crate::isolate_process_tests();
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

    /// Resume each schema-2 boundary without resetting an already captured position.
    #[test]
    fn schema2_interruptions_preserve_the_change_floor() {
        let _isolation = crate::isolate_process_tests();
        for stopped in [
            MigrationStep::JournalPublished,
            MigrationStep::DatabaseCommitted,
            MigrationStep::Checkpointed,
            MigrationStep::MarkerPublished,
            MigrationStep::JournalRemoved,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("metadata");
            let backup = temporary.path().join("before-three");
            let before = schema1_fixture::create_schema2(&root);
            let connection =
                Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            connection
                .execute("UPDATE server_state SET change_seq=42", [])
                .unwrap();
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            connection.close().unwrap();
            assert!(
                migrate_with_hook(&root, &backup, |step| {
                    if step == stopped {
                        Err(StorageError::NeedsRecovery(
                            "injected schema-2 interruption",
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            let resumed = migrate(&root, &backup).unwrap();
            assert_eq!(resumed.metadata.schema_version, SCHEMA_VERSION);
            assert_eq!(resumed.metadata.server_id, before.server_id);
            assert_eq!(resumed.metadata.sync_epoch, before.sync_epoch);
            assert_eq!(resumed.metadata.change_seq.unwrap().value(), 42);
            assert_eq!(
                snapshot::verify_snapshot(&backup).unwrap().schema_version,
                2
            );
        }
    }
}
