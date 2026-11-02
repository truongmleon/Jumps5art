//! Creates and verifies a standalone copy of Jumpstart's metadata database.
//! SQLite's backup API copies the checked database into a private staging directory;
//! a small manifest records its identity, schema, size, and hash before publication.
//!
//! This offline development tool preserves bootstrap or job-ledger metadata.
//! Its bundle contains no media, Worlds state, or secrets. Schema 1 has no change
//! watermark; schemas 2/3 capture their stored value. This remains a local
//! metadata snapshot rather than a complete Jumpstart backup.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use jumpstart_domain::{BlobId, Counter, ServerId, SyncEpoch};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use rustix::fs::RenameFlags;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    VerifiedMetadata, destination, io, open_directory, open_regular, open_verified_metadata,
    read_change_seq, readonly_uri, reject_existing, require_directory, sync_directory,
    verify_database,
};
use crate::{
    FORMAT_VERSION, FormatMarker, Result, SCHEMA_VERSION, SNAPSHOT_DATABASE_FILE,
    SNAPSHOT_FORMAT_VERSION, SNAPSHOT_MANIFEST_FILE, SnapshotReport, SnapshotScope, StorageError,
};

const MANIFEST_LIMIT: u64 = 64 * 1024;
const COPY_TIMEOUT: Duration = Duration::from_secs(60);
const COPY_PAGES_PER_STEP: i32 = 128;
const COPY_RETRY_DELAY: Duration = Duration::from_millis(10);

/// The strict, source-independent description of a published two-file snapshot.
///
/// Identity and schema fields describe the captured database. `database_bytes`
/// and `database_hash` verify its retained bytes. `scope` labels bootstrap data in
/// schemas 1/2 or metadata including jobs in schema 3. `change_seq` is explicitly
/// null for schema 1 or the captured stored counter for schemas 2/3.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    snapshot_format_version: u32,
    scope: SnapshotScope,
    server_id: ServerId,
    sync_epoch: SyncEpoch,
    format_version: u32,
    schema_version: u32,
    #[serde(deserialize_with = "deserialize_change_seq")]
    change_seq: Option<Counter>,
    database_bytes: Counter,
    database_hash: BlobId,
}

/// Require the watermark field to be present even though bootstrap uses JSON null.
///
/// `deserializer: D` supplies the explicit field value through `Deserializer<'de>`.
/// Returns `Result<Option<Counter>, D::Error>`; schema-specific checks distinguish
/// absent storage in schema 1 from the actual stored counter in schemas 2/3.
/// Using this field callback prevents Serde from treating a missing field as `None`.
fn deserialize_change_seq<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Counter>, D::Error> {
    Option::<Counter>::deserialize(deserializer)
}

/// Capture a checked, offline metadata root and publish a verified snapshot bundle.
///
/// # Arguments
/// - `root: &Path`: an initialized, checkpointed source protected by its maintenance lock.
/// - `output: &Path`: a nonexistent destination outside the source root, with an existing parent.
///
/// # Returns
/// `Result<SnapshotReport>` describing the verified, published two-file snapshot.
///
/// # Errors
/// Refuses unsupported/active/corrupt sources and unsafe/existing destinations.
/// Copy, verification, or publication failures retain private staging when present;
/// an error after the final rename can leave a published bundle for inspection.
pub(crate) fn create_snapshot(root: &Path, output: &Path) -> Result<SnapshotReport> {
    // Keep the whole guard, including its source connection and shared lock, alive
    // through publication. An inspect-then-reopen sequence would lose that protection.
    let source = open_verified_metadata(root)?;
    create_snapshot_from_verified(&source, output)
}

/// Capture a checked source while retaining its existing shared or exclusive lock.
///
/// # Arguments
/// * `source` — `&VerifiedMetadata` owned by inspection or the migration workflow.
/// * `output` — new snapshot-directory `&Path` outside the source root.
///
/// # Returns
/// `Result<SnapshotReport>` after the copied database and saved manifest agree.
///
/// # Errors
/// Propagates the same path, copy, verification, and publication failures as
/// `create_snapshot`. It never acquires another lock, so migration can retain its
/// exclusive source lock without downgrading it while creating its safety copy.
pub(super) fn create_snapshot_from_verified(
    source: &VerifiedMetadata,
    output: &Path,
) -> Result<SnapshotReport> {
    let (parent, name) = destination(output)?;
    let final_root = parent.join(&name);
    if final_root.starts_with(&source.report.root) {
        return Err(StorageError::InvalidSnapshot(
            "snapshot destination must be outside the source metadata root",
        ));
    }
    reject_existing(&final_root)?;
    let parent_handle = open_directory(&parent)?;
    let stage_name = format!(".jumpstart-snapshot-{}", Uuid::new_v4());
    let staging = parent.join(&stage_name);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|error| io("create private snapshot staging", &staging, error))?;

    let outcome = (|| {
        let database_path = staging.join(SNAPSHOT_DATABASE_FILE);
        copy_database(
            &source.connection,
            &database_path,
            &source.marker,
            Instant::now() + COPY_TIMEOUT,
        )?;
        let (database_bytes, database_hash) = hash_database(&database_path)?;
        let manifest = SnapshotManifest {
            snapshot_format_version: SNAPSHOT_FORMAT_VERSION,
            scope: scope_for_schema(source.marker.schema_version)?,
            server_id: source.marker.server_id,
            sync_epoch: source.marker.sync_epoch,
            format_version: source.marker.format_version,
            schema_version: source.marker.schema_version,
            change_seq: read_change_seq(&source.connection, source.marker.schema_version)?,
            database_bytes,
            database_hash,
        };
        write_manifest(&staging.join(SNAPSHOT_MANIFEST_FILE), &manifest)?;
        sync_directory(&staging)?;
        // Reopen both files through the same non-mutating standalone verifier
        // used after restart; an open destination connection is not enough evidence.
        let mut report = verify_snapshot(&staging)?;
        rustix::fs::renameat_with(
            &parent_handle,
            OsStr::new(&stage_name),
            &parent_handle,
            &name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST || error == rustix::io::Errno::NOTEMPTY {
                StorageError::ExistingTarget(final_root.clone())
            } else {
                io(
                    "publish snapshot without replacement",
                    &final_root,
                    error.into(),
                )
            }
        })?;
        parent_handle
            .sync_all()
            .map_err(|error| io("synchronize snapshot parent", &parent, error))?;
        report.root = final_root;
        Ok(report)
    })();
    outcome.map_err(|error| {
        if staging.exists() {
            StorageError::StagingRetained {
                staging,
                source: Box::new(error),
            }
        } else {
            error
        }
    })
}

/// Verify a snapshot using only its manifest and database, without its source.
///
/// # Arguments
/// - `root: &Path`: a non-symlink directory containing exactly the two snapshot files.
///
/// # Returns
/// `Result<SnapshotReport>` after the declared bytes, schema, and identities agree.
/// No files are created, repaired, checkpointed, or removed.
///
/// # Errors
/// Rejects extra entries (including journals), symlinks, unsupported formats,
/// incorrect/missing watermarks, mismatched hashes/sizes, and invalid SQLite state.
pub(crate) fn verify_snapshot(root: &Path) -> Result<SnapshotReport> {
    require_directory(root)?;
    let root =
        fs::canonicalize(root).map_err(|error| io("resolve snapshot directory", root, error))?;
    verify_entries(&root)?;
    let manifest = read_manifest(&root.join(SNAPSHOT_MANIFEST_FILE))?;
    validate_manifest(&manifest)?;
    let database_path = root.join(SNAPSHOT_DATABASE_FILE);
    let (actual_bytes, actual_hash) = hash_database(&database_path)?;
    if actual_bytes != manifest.database_bytes || actual_hash != manifest.database_hash {
        return Err(StorageError::InvalidSnapshot(
            "snapshot database size or hash does not match its manifest",
        ));
    }
    verify_delete_header(&database_path)?;
    let connection = Connection::open_with_flags(
        readonly_uri(&database_path),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.pragma_update(None, "trusted_schema", false)?;
    let marker = FormatMarker {
        format_version: manifest.format_version,
        schema_version: manifest.schema_version,
        server_id: manifest.server_id,
        sync_epoch: manifest.sync_epoch,
    };
    verify_database(&connection, &marker)?;
    if read_change_seq(&connection, marker.schema_version)? != manifest.change_seq {
        return Err(StorageError::InvalidSnapshot(
            "snapshot change sequence does not match its database",
        ));
    }
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(SnapshotReport {
        root,
        snapshot_format_version: manifest.snapshot_format_version,
        scope: manifest.scope,
        server_id: manifest.server_id,
        sync_epoch: manifest.sync_epoch,
        format_version: manifest.format_version,
        schema_version: manifest.schema_version,
        change_seq: manifest.change_seq,
        database_bytes: manifest.database_bytes,
        database_hash: manifest.database_hash,
    })
}

/// Require the exact two-file scope before opening an immutable SQLite connection.
///
/// `root: &Path` is the checked snapshot directory. Returns `Result<()>` with
/// `Ok(())` only for regular manifest/database files. Missing files, journals,
/// symlinks, directories, and unrelated entries fail without changing the bundle.
fn verify_entries(root: &Path) -> Result<()> {
    let mut manifest_seen = false;
    let mut database_seen = false;
    for entry in fs::read_dir(root).map_err(|error| io("enumerate snapshot files", root, error))? {
        let entry = entry.map_err(|error| io("read snapshot directory entry", root, error))?;
        let name = entry.file_name();
        if name == OsStr::new(SNAPSHOT_MANIFEST_FILE) {
            manifest_seen = true;
        } else if name == OsStr::new(SNAPSHOT_DATABASE_FILE) {
            database_seen = true;
        } else {
            return Err(StorageError::InvalidSnapshot(
                "snapshot must contain only its manifest and database, without journal sidecars",
            ));
        }
        // Check the actual opened object, not just its directory-entry type.
        open_regular(&entry.path())?;
    }
    if !manifest_seen || !database_seen {
        return Err(StorageError::InvalidSnapshot(
            "snapshot manifest or database is missing",
        ));
    }
    Ok(())
}

/// Read a strict JSON snapshot manifest with a fixed allocation/input limit.
///
/// `path: &Path` names the manifest. Returns `Result<SnapshotManifest>` after
/// reading at most 64 KiB plus one detection byte. Oversized, unreadable, or
/// malformed manifests fail; JSON errors use the snapshot-specific error variant.
fn read_manifest(path: &Path) -> Result<SnapshotManifest> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io("read snapshot manifest", path, error))?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(StorageError::InvalidSnapshot(
            "snapshot manifest exceeds 64 KiB",
        ));
    }
    serde_json::from_slice(&bytes).map_err(StorageError::InvalidSnapshotManifest)
}

/// Enforce the artifact format and the schema-specific metadata scope.
///
/// `manifest: &SnapshotManifest` supplies the declared versions and metadata.
/// Returns `Result<()>` or an unsupported/invalid-snapshot error. Schema 1 requires
/// an explicit null watermark; schemas 2/3 require an actual counter whose value
/// the standalone verifier also compares with the copied database.
fn validate_manifest(manifest: &SnapshotManifest) -> Result<()> {
    if manifest.snapshot_format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(StorageError::UnsupportedSnapshotFormat {
            found: manifest.snapshot_format_version,
            supported: SNAPSHOT_FORMAT_VERSION,
        });
    }
    if manifest.format_version != FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormat {
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }
    if !matches!(manifest.schema_version, 1..=3) {
        return Err(StorageError::UnsupportedSchema {
            found: manifest.schema_version.into(),
            supported: SCHEMA_VERSION,
        });
    }
    if manifest.scope != scope_for_schema(manifest.schema_version)?
        || (manifest.schema_version == 1 && manifest.change_seq.is_some())
        || (manifest.schema_version >= 2 && manifest.change_seq.is_none())
        || manifest.database_bytes.value() == 0
    {
        return Err(StorageError::InvalidSnapshot(
            "metadata snapshots require nonempty database bytes and a schema-appropriate change watermark",
        ));
    }
    Ok(())
}

/// Select the exact artifact scope supported by a known database schema.
///
/// `schema` is the captured `u32` layout version. Returns `Result<SnapshotScope>`:
/// bootstrap metadata for 1/2 or metadata including the job ledger for 3. Unknown
/// versions fail rather than silently claiming a broader or narrower backup.
fn scope_for_schema(schema: u32) -> Result<SnapshotScope> {
    match schema {
        1 | 2 => Ok(SnapshotScope::BootstrapMetadata),
        3 => Ok(SnapshotScope::Metadata),
        _ => Err(StorageError::UnsupportedSchema {
            found: schema.into(),
            supported: SCHEMA_VERSION,
        }),
    }
}

/// Reserve a private destination and copy checked metadata using SQLite's API.
///
/// `source: &Connection` is held stable by the caller's metadata guard;
/// `path: &Path` is a new staging file; `marker: &FormatMarker` is its expected
/// captured identity; `deadline: Instant` bounds backup stepping. Returns
/// `Result<()>` after DELETE-mode normalization, database verification, close,
/// and file synchronization; propagates copy, validation, and filesystem failures.
fn copy_database(
    source: &Connection,
    path: &Path,
    marker: &FormatMarker,
    deadline: Instant,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| io("reserve snapshot database", path, error))?;
    let mut destination = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    // Busy/Locked retries are controlled by the monotonic deadline, rather than
    // hidden per-step sleeps that could repeatedly extend that budget.
    destination.busy_timeout(Duration::ZERO)?;
    destination.pragma_update(None, "synchronous", "FULL")?;
    destination.pragma_update(None, "foreign_keys", true)?;
    destination.pragma_update(None, "trusted_schema", false)?;
    destination.pragma_update(None, "journal_mode", "DELETE")?;
    copy_to_completion(source, &mut destination, deadline)?;
    // The copied source uses WAL. Normalize the standalone artifact explicitly
    // after the backup handle is gone, before any final byte hash is recorded.
    let mode: String = destination.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    if mode != "delete" {
        return Err(StorageError::InvalidSnapshot(
            "snapshot database could not enter DELETE journal mode",
        ));
    }
    verify_database(&destination, marker)?;
    destination
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    file.sync_all()
        .map_err(|error| io("synchronize snapshot database", path, error))?;
    verify_delete_header(path)
}

/// Step a backup until SQLite confirms completion or the finite deadline expires.
///
/// `source: &Connection` is the checked source; `destination: &mut Connection`
/// is exclusively borrowed for the backup; `deadline: Instant` is the final
/// permitted step/retry time. Returns `Result<()>` only after `StepResult::Done`.
/// Busy/Locked states receive bounded retries; fatal errors or expiry fail and
/// dropping the handle releases/rolls back any unfinished destination transaction.
fn copy_to_completion(
    source: &Connection,
    destination: &mut Connection,
    deadline: Instant,
) -> Result<()> {
    let backup = Backup::new(source, destination)?;
    loop {
        if Instant::now() >= deadline {
            return Err(StorageError::SnapshotUnavailable(
                "metadata snapshot copy exceeded its time budget",
            ));
        }
        let step = backup.step(COPY_PAGES_PER_STEP)?;
        if Instant::now() >= deadline {
            return Err(StorageError::SnapshotUnavailable(
                "metadata snapshot copy exceeded its time budget",
            ));
        }
        match step {
            StepResult::Done => return Ok(()),
            StepResult::More => std::thread::yield_now(),
            StepResult::Busy | StepResult::Locked => {
                std::thread::sleep(
                    COPY_RETRY_DELAY.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            _ => {
                return Err(StorageError::SnapshotUnavailable(
                    "SQLite reported an unsupported backup step state",
                ));
            }
        }
    }
}

/// Require a complete standalone SQLite header using rollback/DELETE-mode versions.
///
/// `path: &Path` names a regular copied database. Returns `Result<()>` after
/// checking its signature and read/write format bytes, or an I/O/invalid-snapshot
/// error. This check never opens SQLite or creates its journal sidecars.
fn verify_delete_header(path: &Path) -> Result<()> {
    let mut file = open_regular(path)?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header)
        .map_err(|error| io("read snapshot SQLite header", path, error))?;
    if &header[..16] != b"SQLite format 3\0" || header[18] != 1 || header[19] != 1 {
        return Err(StorageError::InvalidSnapshot(
            "snapshot must be a standalone DELETE-mode SQLite database",
        ));
    }
    Ok(())
}

/// Hash one regular database using bounded memory and an explicit byte count.
///
/// `path: &Path` names the database. Returns `Result<(Counter, BlobId)>` with
/// its measured length and tagged BLAKE3 digest. Read failures or a length beyond
/// the signed-64-bit domain range fail rather than truncating the recorded size.
fn hash_database(path: &Path) -> Result<(Counter, BlobId)> {
    let mut file = open_regular(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| io("read snapshot database for hashing", path, error))?;
        if count == 0 {
            break;
        }
        length = length
            .checked_add(count as u64)
            .ok_or(StorageError::InvalidSnapshot(
                "snapshot database length overflowed",
            ))?;
        hasher.update(&buffer[..count]);
    }
    let bytes = Counter::from_u64(length).map_err(|_| {
        StorageError::InvalidSnapshot("snapshot database exceeds the supported byte-count range")
    })?;
    Ok((bytes, BlobId::from_bytes(*hasher.finalize().as_bytes())))
}

/// Write the manifest last inside private staging and synchronize its file bytes.
///
/// `path: &Path` must not exist; `manifest: &SnapshotManifest` describes the
/// completed database. Returns `Result<()>` after a bounded, owner-only JSON file
/// is synchronized. Serialization, collision, size, and I/O failures propagate.
fn write_manifest(path: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(manifest).map_err(StorageError::InvalidSnapshotManifest)?;
    if bytes.len().saturating_add(1) as u64 > MANIFEST_LIMIT {
        return Err(StorageError::InvalidSnapshot(
            "snapshot manifest exceeds 64 KiB",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| io("create snapshot manifest", path, error))?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| io("write snapshot manifest", path, error))?;
    file.sync_all()
        .map_err(|error| io("synchronize snapshot manifest", path, error))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use fs2::FileExt;
    use tempfile::TempDir;

    use super::*;
    use crate::{LOCK_FILE, initialize};

    /// Verify that the checked-source guard holds its shared lock until dropped.
    #[test]
    fn checked_source_guard_keeps_exclusive_writer_blocked_until_drop() {
        let _isolation = crate::isolate_process_tests();
        let parent = TempDir::new().expect("temporary parent");
        let root = parent.path().join("source");
        initialize(&root).expect("initialize fixture metadata");
        let guard = open_verified_metadata(&root).expect("hold checked source");
        let writer = File::open(root.join(LOCK_FILE)).expect("open competing lock handle");
        let error = FileExt::try_lock_exclusive(&writer).expect_err("shared guard blocks writer");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(guard);
        FileExt::try_lock_exclusive(&writer).expect("writer admitted after guard is dropped");
    }

    /// Verify that an expired deadline cannot report a successful SQLite copy.
    #[test]
    fn expired_copy_deadline_fails_and_releases_destination() {
        let _isolation = crate::isolate_process_tests();
        let source = Connection::open_in_memory().expect("source connection");
        source
            .execute_batch(
                "CREATE TABLE example (value TEXT) STRICT; INSERT INTO example VALUES ('keep');",
            )
            .expect("source contents");
        let mut destination = Connection::open_in_memory().expect("destination connection");
        let error = copy_to_completion(&source, &mut destination, Instant::now())
            .expect_err("expired copy must fail");
        assert!(matches!(error, StorageError::SnapshotUnavailable(_)));
        let count: i64 = destination
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'example'",
                [],
                |row| row.get(0),
            )
            .expect("backup handle released destination connection");
        assert_eq!(count, 0);
        let source_value: String = source
            .query_row("SELECT value FROM example", [], |row| row.get(0))
            .expect("source still readable");
        assert_eq!(source_value, "keep");
    }
}
