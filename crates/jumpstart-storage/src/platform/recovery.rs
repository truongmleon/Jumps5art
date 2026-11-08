//! Rebuilds a new local metadata root from an independently verified snapshot.
//!
//! A rollback copy cannot claim the original server's authority or replay its
//! queued work. The restored database gets a fresh server ID and epoch. The
//! input snapshot retains the original job history for inspection.

use std::{
    fs::{self, OpenOptions},
    io::{Read, copy},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

use jumpstart_domain::{BlobId, Counter, ServerId, SyncEpoch};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

use super::{
    destination, finish_root_layout, io, open_regular, prepare_root_layout, publish_metadata_root,
    reject_existing, snapshot, verify_database,
};
use crate::{
    DATABASE_DIRECTORY, DATABASE_FILE, FormatMarker, LOCK_FILE, MetadataReport,
    MetadataRestoreReport, Result, SNAPSHOT_DATABASE_FILE, SnapshotReport, StorageError,
};

/// Verify the selected backup and atomically publish a new offline metadata root.
///
/// `expected_hash` binds the operation to the database the operator inspected.
/// `output` names a new directory. Returns the original snapshot, new verified
/// metadata, and the number of old jobs retained only in the backup.
pub(crate) fn restore_metadata_snapshot(
    snapshot_root: &Path,
    output: &Path,
    expected_hash: BlobId,
) -> Result<MetadataRestoreReport> {
    let (parent, name) = destination(output)?;
    let target = parent.join(name);
    reject_existing(&target)?;
    require_separate_destination(&parent)?;
    let source = snapshot::verify_snapshot(snapshot_root)?;
    if source.database_hash != expected_hash {
        return Err(StorageError::InvalidMetadataRestore(
            "the verified backup digest does not match the expected digest",
        ));
    }
    if target.starts_with(&source.root) {
        return Err(StorageError::InvalidMetadataRestore(
            "the destination must be outside the source backup",
        ));
    }

    let (root, (mut metadata, archived_snapshot_jobs)) =
        publish_metadata_root(&target, "restore", |staging| {
            prepare_restore(staging, &source)
        })?;
    metadata.root = root;
    Ok(MetadataRestoreReport {
        source,
        metadata,
        archived_snapshot_jobs,
    })
}

/// Refuse new directories under an existing installation, even when it is stopped.
fn require_separate_destination(parent: &Path) -> Result<()> {
    for ancestor in parent.ancestors() {
        let lock = ancestor.join(LOCK_FILE);
        match fs::symlink_metadata(&lock) {
            Ok(_) => {
                return Err(StorageError::InvalidMetadataRestore(
                    "the destination must be outside any existing metadata root",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io(
                    "inspect destination for an existing metadata root",
                    &lock,
                    error,
                ));
            }
        }
    }
    Ok(())
}

/// Copy and recheck the captured bytes in private staging before rewriting authority.
///
/// `source` describes the verified snapshot. Returns the checked staged root
/// and old-job count. A failed preparation leaves only private staging.
fn prepare_restore(staging: &Path, source: &SnapshotReport) -> Result<(MetadataReport, Counter)> {
    let lock = prepare_root_layout(staging)?;
    let source_database = source.root.join(SNAPSHOT_DATABASE_FILE);
    let path = staging.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
    let mut database = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| io("reserve restored database", &path, error))?;
    copy(
        &mut open_regular(&source_database)?.take(source.database_bytes.value() + 1),
        &mut database,
    )
    .map_err(|error| io("copy snapshot database into staging", &path, error))?;
    if snapshot::hash_database(&path)? != (source.database_bytes, source.database_hash) {
        return Err(StorageError::InvalidMetadataRestore(
            "backup bytes changed after verification",
        ));
    }
    let before = FormatMarker {
        format_version: source.format_version,
        schema_version: source.schema_version,
        server_id: source.server_id,
        sync_epoch: source.sync_epoch,
    };
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    verify_database(&connection, &before)?;
    if source.schema_version >= 4
        && connection.query_row("SELECT EXISTS(SELECT 1 FROM volumes)", [], |row| {
            row.get::<_, bool>(0)
        })?
    {
        return Err(StorageError::InvalidMetadataRestore(
            "registered storage requires a complete recovery generation",
        ));
    }

    let marker = FormatMarker {
        server_id: ServerId::new(),
        sync_epoch: SyncEpoch::new(),
        ..before
    };
    let archived = replace_authority(&mut connection, &marker)?;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    database
        .sync_all()
        .map_err(|error| io("synchronize restored database", &path, error))?;
    let report = finish_root_layout(staging, &marker, lock)?;
    Ok((report, archived))
}

/// Start a new authority and fence all old local job work in one transaction.
///
/// The backup is unchanged. The copy retains the captured sequence as its new
/// history floor and leaves saved job projections in the original epoch only.
fn replace_authority(connection: &mut Connection, marker: &FormatMarker) -> Result<Counter> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let journal: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if journal != "wal" {
        return Err(StorageError::InvalidMetadataRestore(
            "the restored database could not enter WAL mode",
        ));
    }
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE server_state SET server_id=?1, sync_epoch=?2 WHERE singleton=1",
        params![
            marker.server_id.as_uuid().as_bytes().as_slice(),
            marker.sync_epoch.as_uuid().as_bytes().as_slice()
        ],
    )?;
    let mut archived = Counter::ZERO;
    if marker.schema_version >= 3 {
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))?;
        archived = u64::try_from(count)
            .ok()
            .and_then(|value| Counter::from_u64(value).ok())
            .ok_or(StorageError::Integrity("invalid recovered job count"))?;
        transaction.execute_batch(
            "DELETE FROM change_batches;
             DELETE FROM jobs;
             UPDATE server_state SET change_floor=change_seq WHERE singleton=1;",
        )?;
    }
    transaction.commit()?;
    verify_database(connection, marker)?;
    let busy: i64 =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(StorageError::InvalidMetadataRestore(
            "the restored database did not checkpoint",
        ));
    }
    Ok(archived)
}
