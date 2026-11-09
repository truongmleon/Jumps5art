//! Performs metadata setup and checks using Linux and macOS filesystem operations.
//! The metadata is server bookkeeping, such as its ID and database layout version.
//!
//! For initialization, this code prepares the SQLite database and format.json in
//! a temporary working folder, called staging. After checking them, it renames the
//! folder into the requested location without replacing anything already there.
//! This keeps unfinished preparation separate from the final metadata root.
//!
//! For inspection, it checks existing files without repairing them. Locks coordinate
//! inspection with cooperating writers. File handles, requests to flush changes
//! to disk, and rename operations depend on the operating system, so they stay
//! in this module behind the public initialize/inspect functions in lib.rs.
//!
//! The snapshot submodule reuses the checked reader and its lock to copy this
//! bookkeeping into a separate verified bundle, without copying library media.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use jumpstart_domain::{Counter, ServerId, SyncEpoch};
use rusqlite::{Connection, OpenFlags, params};
use rustix::fs::{Mode, OFlags, RenameFlags};
use uuid::Uuid;

use super::{
    APPLICATION_ID, DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, FORMAT_VERSION, FormatMarker,
    LOCK_FILE, MIGRATION_FILE, MetadataReport, Result, SCHEMA_VERSION, StorageError,
};

mod changes;
mod history;
mod jobs;
mod migration;
mod schema3;
mod schema4;
mod schema5;
mod schema6;
mod schema7;
mod schema8;
mod snapshot;
#[cfg(target_os = "linux")]
mod worlds;
#[cfg(target_os = "linux")]
pub use worlds::{WorldsCatalogStatus, WorldsRecoveryIndex, WorldsRecoveryPoint};

pub(super) use changes::pull_job_changes;
pub(super) use history::job_history;
pub(super) use jobs::{
    cancel_job, enqueue_snapshot_job, inspect_job, list_jobs, list_jobs_in_state, recover_jobs,
    run_next_snapshot_job, run_snapshot_job,
};
pub(super) use migration::migrate;
pub(super) use snapshot::{create_snapshot, verify_snapshot};

// The Linux daemon is the device authority writer.
#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
pub use devices::{DevicePage, DeviceRecord, DeviceRegistration, device_fingerprint};

// Mass uses the same checked writer lease and registered roots as local administration.
#[cfg(target_os = "linux")]
mod mass;
// Linux registration owns filesystem mutations and observations for the admin lease.
#[cfg(target_os = "linux")]
mod registration;
#[cfg(target_os = "linux")]
pub use registration::{
    RegisteredHardwareTargets, RegisteredVolume, RegisteredVolumeHealth, VolumeRegistrationPlan,
};

const MIGRATION_NAME: &str = "initial_metadata";
const MIGRATIONS_TABLE: &str = "CREATE TABLE schema_migrations (\n    version INTEGER PRIMARY KEY CHECK (version > 0),\n    name TEXT NOT NULL,\n    checksum TEXT NOT NULL,\n    applied_at TEXT NOT NULL\n) STRICT";
const STATE_TABLE: &str = "CREATE TABLE server_state (\n    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n    format_version INTEGER NOT NULL CHECK (format_version > 0),\n    server_id BLOB NOT NULL CHECK (length(server_id) = 16),\n    sync_epoch BLOB NOT NULL CHECK (length(sync_epoch) = 16)\n) STRICT";
const STATE_TABLE_V2: &str = "CREATE TABLE server_state (\n    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n    format_version INTEGER NOT NULL CHECK (format_version > 0),\n    server_id BLOB NOT NULL CHECK (length(server_id) = 16),\n    sync_epoch BLOB NOT NULL CHECK (length(sync_epoch) = 16)\n, change_seq INTEGER NOT NULL DEFAULT 0 CHECK (change_seq >= 0)) STRICT";
const STATE_TABLE_V3: &str = "CREATE TABLE server_state (\n    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n    format_version INTEGER NOT NULL CHECK (format_version > 0),\n    server_id BLOB NOT NULL CHECK (length(server_id) = 16),\n    sync_epoch BLOB NOT NULL CHECK (length(sync_epoch) = 16)\n, change_seq INTEGER NOT NULL DEFAULT 0 CHECK (change_seq >= 0), change_floor INTEGER NOT NULL DEFAULT 0 CHECK (change_floor >= 0 AND change_floor <= change_seq)) STRICT";
const WATERMARK_MIGRATION_NAME: &str = "change_watermark";
const WATERMARK_MIGRATION: &str = "ALTER TABLE server_state ADD COLUMN change_seq INTEGER NOT NULL DEFAULT 0 CHECK (change_seq >= 0)";
const MARKER_LIMIT: u64 = 64 * 1024;

/// Keeps a checked metadata connection and its maintenance lock alive together.
///
/// Ordinary inspection holds a shared lock and immutable reader. A local snapshot
/// job holds an exclusive lock and checked writer, committing each status before
/// copying with no active write transaction. Keeping these resources together
/// prevents a capture from losing protection between checking and reading data.
struct VerifiedMetadata {
    /// Checked source identity, versions, and canonical root path.
    report: MetadataReport,
    /// Checked format marker used when validating the copied database.
    marker: FormatMarker,
    /// Checked reader or exclusive job writer; capture uses no active write transaction.
    connection: Connection,
    /// Owned lock handle retained until capture or inspection finishes.
    _lock: File,
}

/// Retains exclusive ownership of the verified metadata root while the daemon runs.
///
/// The daemon starts with a checked reader and opens a writer only on demand.
/// The same lock excludes offline migrations and job runners in both modes.
#[cfg(target_os = "linux")]
pub struct AdminMetadata {
    source: VerifiedMetadata,
    directory: File,
    database: File,
    verified_at: std::time::Instant,
    writable: bool,
}

#[cfg(target_os = "linux")]
impl AdminMetadata {
    /// Open an existing root and retain its exclusive maintenance lock.
    ///
    /// `root` selects the directory. Returns the verified lease or a storage
    /// error. Missing roots, pending recovery, and active owners are refused.
    pub fn open(root: &Path) -> Result<Self> {
        let (root, lock) = lock_metadata(root, true)?;
        let directory = open_directory(&root)?;
        let database = open_regular(&root.join(DATABASE_DIRECTORY).join(DATABASE_FILE))?;
        let lease = Self {
            source: verify_locked_metadata(root, lock)?,
            directory,
            database,
            verified_at: std::time::Instant::now(),
            writable: false,
        };
        lease.check_binding()?;
        Ok(lease)
    }

    /// Return the verified startup report while the metadata lease is held.
    pub fn report(&self) -> &MetadataReport {
        &self.source.report
    }

    /// Recheck that the paths still name the leased directory, database, and lock.
    ///
    /// Returns `Ok(())` for the same entries. A missing or replaced path means
    /// the startup verification cannot stand in for current metadata evidence.
    pub fn check_binding(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let root = &self.source.report.root;
        let pairs = [
            (&self.directory, open_directory(root)?),
            (
                &self.database,
                open_regular(&root.join(DATABASE_DIRECTORY).join(DATABASE_FILE))?,
            ),
            (&self.source._lock, open_regular(&root.join(LOCK_FILE))?),
        ];
        for (retained, current) in pairs {
            let retained = retained
                .metadata()
                .map_err(|error| io("stat leased metadata", root, error))?;
            let current = current
                .metadata()
                .map_err(|error| io("stat current metadata", root, error))?;
            if (retained.dev(), retained.ino()) != (current.dev(), current.ino()) {
                return Err(StorageError::NeedsRecovery(
                    "metadata path no longer names the leased files",
                ));
            }
        }
        Ok(())
    }

    /// Return the elapsed time since this process verified metadata at startup.
    pub fn verification_age(&self) -> Duration {
        self.verified_at.elapsed()
    }

    /// Read the persisted fence, or return `None` for an older metadata schema.
    pub fn maintenance(&self) -> Result<Option<bool>> {
        self.check_binding()?;
        if self.source.report.schema_version < 5 {
            return Ok(None);
        }
        Ok(Some(self.source.connection.query_row(
            "SELECT maintenance FROM local_admin_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?))
    }

    /// Persist the local mutation fence before reporting the requested state.
    pub fn set_maintenance(&mut self, enabled: bool) -> Result<bool> {
        let previous = self.maintenance()?.ok_or(StorageError::AdminUnavailable(
            "migrate offline metadata before changing maintenance mode",
        ))?;
        if previous == enabled && !self.writable {
            return Ok(enabled);
        }
        self.ensure_writer()?;
        self.source.connection.execute(
            "UPDATE local_admin_state SET maintenance=?1 WHERE singleton=1 AND maintenance!=?1",
            [enabled],
        )?;
        self.checkpoint_admin()?;
        Ok(enabled)
    }

    /// Switch to a checked SQLite writer while retaining the startup lease.
    fn ensure_writer(&mut self) -> Result<()> {
        self.check_binding()?;
        if self.writable {
            return Ok(());
        }
        let root = &self.source.report.root;
        jobs::checked_sidecars(root, false)?;
        let connection = Connection::open_with_flags(
            root.join(DATABASE_DIRECTORY).join(DATABASE_FILE),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        use rusqlite::config::DbConfig;
        if !connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)? {
            return Err(StorageError::AdminUnavailable(
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
            return Err(StorageError::AdminUnavailable(
                "the administration writer requires WAL mode",
            ));
        }
        verify_database(&connection, &self.source.marker)?;
        self.check_binding()?;
        self.source.connection = connection;
        self.writable = true;
        Ok(())
    }

    /// Flush committed changes and their directory before acknowledging a write.
    fn checkpoint_admin(&self) -> Result<()> {
        self.check_binding()?;
        let busy: i64 =
            self.source
                .connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(StorageError::NeedsRecovery(
                "administration checkpoint is busy",
            ));
        }
        let root = &self.source.report.root;
        self.database
            .sync_all()
            .map_err(|error| io("synchronize administration database", root, error))?;
        sync_directory(&root.join(DATABASE_DIRECTORY))
    }

    /// Count saved snapshot jobs by state while the current metadata binding holds.
    ///
    /// Returns `None` for supported roots older than the local job ledger. Counts
    /// describe saved jobs and use exact counters for JSON serialization.
    pub fn snapshot_job_counts(
        &self,
    ) -> Result<Option<std::collections::BTreeMap<String, Counter>>> {
        self.check_binding()?;
        if self.source.report.schema_version < 3 {
            return Ok(None);
        }
        let mut statement = self
            .source
            .connection
            .prepare("SELECT state, count(*) FROM jobs GROUP BY state")?;
        let mut rows = statement.query([])?;
        let mut counts = std::collections::BTreeMap::new();
        while let Some(row) = rows.next()? {
            let count: i64 = row.get(1)?;
            let count = u64::try_from(count)
                .ok()
                .and_then(|count| Counter::from_u64(count).ok())
                .ok_or(StorageError::Integrity("invalid saved job count"))?;
            counts.insert(row.get(0)?, count);
        }
        Ok(Some(counts))
    }
}

/// Create and verify a sibling staging root, then publish it without replacement.
///
/// # Arguments
///
/// * `root` — `&Path` naming a new child of an existing parent directory.
///
/// # Returns
///
/// `Result<MetadataReport>` with the published root's path and verified identity.
///
/// # Errors
///
/// Propagates path, I/O, database, and verification failures. Failed staging is
/// retained when present. If synchronizing the parent fails after publication,
/// the destination remains in place even though this function returns an error.
pub(super) fn initialize(root: &Path) -> Result<MetadataReport> {
    let (parent, name) = destination(root)?;
    let final_root = parent.join(&name);
    reject_existing(&final_root)?;
    let parent_handle = open_directory(&parent)?;
    let stage_name = format!(".jumpstart-init-{}", Uuid::new_v4());
    let staging = parent.join(&stage_name);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|source| io("create private staging directory", &staging, source))?;

    // Do not remove failed or interrupted staging. Nothing in a user's existing
    // directory is ever adopted, overwritten, renamed, or deleted here.
    let outcome = prepare_staging(&staging).and_then(|mut report| {
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
                    "publish metadata without replacement",
                    &final_root,
                    error.into(),
                )
            }
        })?;
        // A sync failure after publication is still an error, not a false claim
        // of durable success. Retain the complete published root for inspection.
        parent_handle
            .sync_all()
            .map_err(|source| io("synchronize metadata parent", &parent, source))?;
        report.root = final_root.clone();
        Ok(report)
    });
    outcome.map_err(|source| {
        if staging.exists() {
            StorageError::StagingRetained {
                staging,
                source: Box::new(source),
            }
        } else {
            source
        }
    })
}

/// Populate a private staging directory and verify it through offline inspection.
///
/// # Arguments
///
/// * `staging` — `&Path` of the newly created, empty staging directory.
///
/// # Returns
///
/// `Result<MetadataReport>` describing the verified staging root before publication.
///
/// # Errors
///
/// Propagates failures creating, locking, writing, synchronizing, or verifying the
/// staged files. Partially prepared files remain available for diagnosis.
fn prepare_staging(staging: &Path) -> Result<MetadataReport> {
    let lock_path = staging.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|source| io("create maintenance lock", &lock_path, source))?;
    FileExt::try_lock_exclusive(&lock)
        .map_err(|source| io("lock new metadata root", &lock_path, source))?;
    lock.sync_all()
        .map_err(|source| io("synchronize maintenance lock", &lock_path, source))?;

    for directory in [DATABASE_DIRECTORY, "secrets", "diagnostics", "snapshots"] {
        let path = staging.join(directory);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|source| io("create metadata directory", &path, source))?;
        sync_directory(&path)?;
    }
    let marker = FormatMarker {
        format_version: FORMAT_VERSION,
        schema_version: SCHEMA_VERSION,
        server_id: ServerId::new(),
        sync_epoch: SyncEpoch::new(),
    };
    let database_path = staging.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
    create_database(&database_path, &marker)?;
    let marker_path = staging.join(FORMAT_FILE);
    let mut marker_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&marker_path)
        .map_err(|source| io("create format marker", &marker_path, source))?;
    serde_json::to_writer_pretty(&mut marker_file, &marker)?;
    marker_file
        .write_all(b"\n")
        .map_err(|source| io("write format marker", &marker_path, source))?;
    marker_file
        .sync_all()
        .map_err(|source| io("synchronize format marker", &marker_path, source))?;
    sync_directory(&staging.join(DATABASE_DIRECTORY))?;
    sync_directory(staging)?;
    // Verify under the existing exclusive lock and release it explicitly.
    // A concurrent fork can briefly retain a close-on-exec descriptor.
    let root = fs::canonicalize(staging)
        .map_err(|source| io("resolve staged metadata root", staging, source))?;
    let verified = verify_locked_metadata(root, lock)?;
    FileExt::unlock(&verified._lock).map_err(|source| {
        io(
            "release staged maintenance lock",
            &staging.join(LOCK_FILE),
            source,
        )
    })?;
    Ok(verified.report)
}

/// Create the initial SQLite database, commit its schema and identity, and checkpoint it.
///
/// # Arguments
///
/// * `path` — `&Path` at which a new owner-only database file is reserved.
/// * `marker` — `&FormatMarker` supplying the versions and identity stored in SQLite.
///
/// # Returns
///
/// `Result<()>` with `Ok(())` after verification, checkpointing, closure, and file sync.
///
/// # Errors
///
/// Returns a storage error for file creation or synchronization, SQLite operations,
/// unapplied settings, verification failure, or an incomplete WAL checkpoint.
/// This helper does not publish or clean up the enclosing staging directory.
fn create_database(path: &Path, marker: &FormatMarker) -> Result<()> {
    // SQLite otherwise uses the process umask for its initial database. Reserve
    // the file exclusively with owner-only access before opening it without CREATE.
    let database = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| io("reserve SQLite database", path, source))?;
    database
        .sync_all()
        .map_err(|source| io("synchronize empty database", path, source))?;
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    let journal: String = connection.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    let foreign_keys: i64 = connection.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    let checkpoint: i64 =
        connection.pragma_query_value(None, "wal_autocheckpoint", |r| r.get(0))?;
    if journal != "wal" || synchronous != 2 || foreign_keys != 1 || checkpoint != 0 {
        return Err(StorageError::Integrity(
            "required SQLite settings were not applied",
        ));
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(MIGRATIONS_TABLE)?;
    transaction.execute_batch(STATE_TABLE)?;
    transaction.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![1, MIGRATION_NAME, migration_checksum()],
    )?;
    transaction.execute(
        "INSERT INTO server_state (singleton, format_version, server_id, sync_epoch) VALUES (1, ?1, ?2, ?3)",
        params![FORMAT_VERSION, marker.server_id.as_uuid().as_bytes().as_slice(), marker.sync_epoch.as_uuid().as_bytes().as_slice()],
    )?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", 1)?;
    transaction.commit()?;
    apply_schema2(&mut connection)?;
    apply_schema3(&mut connection)?;
    schema4::apply(&mut connection)?;
    schema5::apply(&mut connection)?;
    schema6::apply(&mut connection)?;
    schema7::apply(&mut connection)?;
    schema8::apply(&mut connection)?;
    verify_database(&connection, marker)?;
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 {
        return Err(StorageError::NeedsRecovery(
            "initial WAL checkpoint could not complete",
        ));
    }
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    database
        .sync_all()
        .map_err(|source| io("synchronize initialized database", path, source))?;
    Ok(())
}

/// Verify a checkpointed metadata root while holding a shared maintenance lock.
///
/// # Arguments
///
/// * `root` — `&Path` to an existing offline metadata root.
///
/// # Returns
///
/// `Result<MetadataReport>` with the canonical path, matched identity, and versions.
///
/// # Errors
///
/// Rejects invalid paths/files, active exclusive writers, nonempty journals,
/// unsupported versions, invalid SQLite headers, and inconsistent schema or identity.
/// Propagates I/O and database failures without recovering or repairing the root.
pub(super) fn inspect(root: &Path) -> Result<MetadataReport> {
    let VerifiedMetadata {
        report,
        connection,
        _lock,
        ..
    } = open_verified_metadata(root)?;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(report)
}

/// Open and validate an offline source while retaining its maintenance lock.
///
/// # Arguments
///
/// * `root` — `&Path` of an existing, checkpointed metadata root.
///
/// # Returns
///
/// `Result<VerifiedMetadata>` owning the checked marker, report, connection, and
/// shared lock. The caller must retain the guard while reading/copying the source.
///
/// # Errors
///
/// Returns the same path, version, recovery, integrity, I/O, and SQLite failures
/// as offline inspection. It never opens an active WAL database as immutable.
fn open_verified_metadata(root: &Path) -> Result<VerifiedMetadata> {
    let (root, lock) = lock_metadata(root, false)?;
    verify_locked_metadata(root, lock)
}

/// Retain a shared diagnostic lock or an exclusive maintenance lock on a root.
///
/// # Arguments
/// * `root` — existing metadata `&Path`; its root, database folder, and lock must exist.
/// * `exclusive` — `bool` selecting exclusive maintenance instead of shared reading.
///
/// # Returns
/// `Result<(PathBuf, File)>` with the canonical root and the owned locked handle.
///
/// # Errors
/// Refuses invalid paths, an unavailable lock, or failed filesystem operations.
/// It does not create the lock or inspect/recover an interrupted migration.
fn lock_metadata(root: &Path, exclusive: bool) -> Result<(PathBuf, File)> {
    require_directory(root)?;
    let root =
        fs::canonicalize(root).map_err(|source| io("resolve metadata root", root, source))?;
    let database_directory = root.join(DATABASE_DIRECTORY);
    require_directory(&database_directory)?;
    let lock_path = root.join(LOCK_FILE);
    let lock = open_regular(&lock_path)?;
    let outcome = if exclusive {
        FileExt::try_lock_exclusive(&lock)
    } else {
        FileExt::try_lock_shared(&lock)
    };
    outcome.map_err(|source| {
        if source.kind() == std::io::ErrorKind::WouldBlock {
            StorageError::InUse
        } else {
            io("acquire metadata maintenance lock", &lock_path, source)
        }
    })?;
    Ok((root, lock))
}

/// Read the root's bounded format marker and reject unsupported directory/schema versions.
///
/// # Arguments
/// * `root` — `&Path` of an existing root whose caller retains the maintenance lock.
///
/// # Returns
/// `Result<FormatMarker>` containing a supported schema-1, -2, or -3 marker.
///
/// # Errors
/// Returns I/O/JSON failures or unsupported versions. Database identity and schema
/// verification are separate because migration recovery may inspect a pending marker.
fn read_marker(root: &Path) -> Result<FormatMarker> {
    let marker_path = root.join(FORMAT_FILE);
    let marker_file = open_regular(&marker_path)?;
    if marker_file
        .metadata()
        .map_err(|source| io("inspect format marker length", &marker_path, source))?
        .len()
        > MARKER_LIMIT
    {
        return Err(StorageError::Integrity("format marker exceeds 64 KiB"));
    }
    let marker: FormatMarker = serde_json::from_reader(marker_file.take(MARKER_LIMIT + 1))?;
    if marker.format_version != FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormat {
            found: marker.format_version,
            supported: FORMAT_VERSION,
        });
    }
    if !(1..=SCHEMA_VERSION).contains(&marker.schema_version) {
        return Err(StorageError::UnsupportedSchema {
            found: marker.schema_version.into(),
            supported: SCHEMA_VERSION,
        });
    }
    Ok(marker)
}

/// Check an offline root using a lock already held by inspection or maintenance.
///
/// # Arguments
/// * `root` — canonical `PathBuf` returned by `lock_metadata`.
/// * `lock` — owned `File` retaining the shared or exclusive maintenance lock.
///
/// # Returns
/// `Result<VerifiedMetadata>` retaining the same lock and checked read connection.
///
/// # Errors
/// Refuses a migration journal, unsupported marker, pending WAL/rollback journal,
/// malformed database, or mismatched identity/schema. No files are repaired.
fn verify_locked_metadata(root: PathBuf, lock: File) -> Result<VerifiedMetadata> {
    match fs::symlink_metadata(root.join(MIGRATION_FILE)) {
        Ok(_) => {
            return Err(StorageError::NeedsRecovery(
                "metadata migration journal is present; resume the explicit migration before inspection",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io(
                "inspect migration journal",
                &root.join(MIGRATION_FILE),
                error,
            ));
        }
    }
    let marker = read_marker(&root)?;
    let database_directory = root.join(DATABASE_DIRECTORY);
    let database_path = database_directory.join(DATABASE_FILE);
    for extension in ["-wal", "-journal"] {
        let sidecar = database_directory.join(format!("{DATABASE_FILE}{extension}"));
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(StorageError::InvalidPath {
                    path: sidecar,
                    reason: "journal sidecars must be regular non-symlink files",
                });
            }
            Ok(metadata) if metadata.len() > 0 => {
                return Err(StorageError::NeedsRecovery(
                    "nonempty WAL or rollback journal; stop the writer and checkpoint/recover before inspection",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("inspect journal sidecar", &sidecar, error)),
        }
    }
    let mut database_file = open_regular(&database_path)?;
    let mut header = [0_u8; 100];
    database_file
        .read_exact(&mut header)
        .map_err(|source| io("read SQLite header", &database_path, source))?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(StorageError::Integrity("not a SQLite database"));
    }
    if header[18] != 2 || header[19] != 2 {
        return Err(StorageError::Integrity(
            "database is not configured for WAL mode",
        ));
    }
    let uri = readonly_uri(&database_path);
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    verify_database(&connection, &marker)?;
    Ok(VerifiedMetadata {
        report: MetadataReport {
            root,
            server_id: marker.server_id,
            sync_epoch: marker.sync_epoch,
            format_version: marker.format_version,
            schema_version: marker.schema_version,
            change_seq: read_change_seq(&connection, marker.schema_version)?,
            journal_mode: "wal".to_owned(),
            integrity_check: "ok".to_owned(),
        },
        marker,
        connection,
        _lock: lock,
    })
}

/// Check SQLite integrity, exact schema and migration records, and marker identity.
///
/// # Arguments
///
/// * `connection` — `&Connection` whose existing database is queried for verification.
/// * `marker` — `&FormatMarker` supplying the expected format and identity.
///
/// # Returns
///
/// `Result<()>` with `Ok(())` when all database checks match this binary and marker.
///
/// # Errors
///
/// Propagates query/conversion errors and rejects unsupported schemas, corruption,
/// foreign-key violations, migration/schema mismatches, or differing identities.
fn verify_database(connection: &Connection, marker: &FormatMarker) -> Result<()> {
    let application: i32 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if application != APPLICATION_ID {
        return Err(StorageError::Integrity(
            "SQLite application_id does not identify Jumpstart metadata",
        ));
    }
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !(1..=i64::from(SCHEMA_VERSION)).contains(&version) {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version != i64::from(marker.schema_version) {
        return Err(StorageError::MigrationMismatch);
    }
    verify_schema_objects(connection, marker.schema_version)?;
    let checks = connection
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if checks != ["ok"] {
        return Err(StorageError::Integrity(
            "SQLite integrity_check reported corruption",
        ));
    }
    if connection
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        return Err(StorageError::Integrity(
            "SQLite foreign_key_check reported a violation",
        ));
    }
    let migrations = connection
        .prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut expected_migrations = vec![(1, MIGRATION_NAME.to_owned(), migration_checksum())];
    if version >= 2 {
        expected_migrations.push((
            2,
            WATERMARK_MIGRATION_NAME.to_owned(),
            watermark_migration_checksum(),
        ));
    }
    if version >= 3 {
        expected_migrations.push((3, schema3::MIGRATION_NAME.to_owned(), schema3::checksum()));
    }
    if version >= 4 {
        expected_migrations.push((4, schema4::MIGRATION_NAME.to_owned(), schema4::checksum()));
    }
    if version >= 5 {
        expected_migrations.push((5, schema5::MIGRATION_NAME.to_owned(), schema5::checksum()));
        if version == 5 {
            schema5::verify_state(connection)?;
        }
    }
    if version >= 6 {
        expected_migrations.push((6, schema6::MIGRATION_NAME.to_owned(), schema6::checksum()));
        schema6::verify_state(connection)?;
    }
    if version >= 7 {
        expected_migrations.push((7, schema7::MIGRATION_NAME.to_owned(), schema7::checksum()));
        schema7::verify_state(connection)?;
    }
    if version >= 8 {
        expected_migrations.push((8, schema8::MIGRATION_NAME.to_owned(), schema8::checksum()));
    }
    if migrations != expected_migrations {
        return Err(StorageError::MigrationMismatch);
    }
    let (format_version, server_id, sync_epoch): (i64, Vec<u8>, Vec<u8>) = connection.query_row(
        "SELECT format_version, server_id, sync_epoch FROM server_state WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if format_version != i64::from(marker.format_version)
        || server_id.as_slice() != marker.server_id.as_uuid().as_bytes()
        || sync_epoch.as_slice() != marker.sync_epoch.as_uuid().as_bytes()
    {
        return Err(StorageError::IdentityMismatch);
    }
    read_change_seq(connection, marker.schema_version)?;
    if version >= 3 {
        schema3::verify_ledger(connection)?;
        jobs::verify_all_jobs(connection)?;
    }
    Ok(())
}

/// Require the complete DDL for the database's declared version.
///
/// `connection` is the SQLite database and `version` has already been checked
/// against the marker. Returns `Ok(())` only for the exact set of application
/// objects. This catches a missing constraint or index even when the migration
/// checksum row itself was left untouched.
fn verify_schema_objects(connection: &Connection, version: u32) -> Result<()> {
    let mut expected = vec![
        MIGRATIONS_TABLE,
        match version {
            1 => STATE_TABLE,
            2 => STATE_TABLE_V2,
            _ => STATE_TABLE_V3,
        },
    ];
    if version >= 3 {
        expected.extend([
            schema3::JOBS_TABLE,
            schema3::CHANGE_BATCHES_TABLE,
            schema3::JOBS_QUEUE_INDEX,
        ]);
    }
    if version >= 4 {
        expected.extend(schema4::statements());
    }
    match version {
        5 => expected.extend(schema5::statements()),
        6.. => expected.extend(schema6::statements()),
        _ => {}
    }
    if version >= 7 {
        expected.extend(schema7::statements());
    }
    if version >= 8 {
        expected.extend(schema8::statements());
    }
    expected.sort_unstable();
    let actual = connection
        .prepare("SELECT sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY sql")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err(StorageError::MigrationMismatch);
    }
    Ok(())
}

/// Read the stored change high-water mark without inventing one for schema 1.
///
/// # Arguments
/// * `connection` — checked `&Connection` containing the selected schema.
/// * `schema` — `u32` a supported schema version.
///
/// # Returns
/// `Result<Option<Counter>>`: `None` for schema 1, or the actual nonnegative value
/// for schema 2 and later. Zero means that no sequenced mutations have been committed yet.
///
/// # Errors
/// Refuses unsupported versions, missing/invalid values, and failed queries.
fn read_change_seq(connection: &Connection, schema: u32) -> Result<Option<Counter>> {
    match schema {
        1 => Ok(None),
        2..=SCHEMA_VERSION => {
            let sequence: i64 = connection.query_row(
                "SELECT change_seq FROM server_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )?;
            let sequence = u64::try_from(sequence)
                .ok()
                .and_then(|value| Counter::from_u64(value).ok())
                .ok_or(StorageError::Integrity("invalid stored change sequence"))?;
            Ok(Some(sequence))
        }
        _ => Err(StorageError::UnsupportedSchema {
            found: schema.into(),
            supported: SCHEMA_VERSION,
        }),
    }
}

/// Apply the checksummed schema-2 watermark migration to a verified schema-1 database.
///
/// # Arguments
/// * `connection` — mutable `&mut Connection` held by the exclusive maintenance owner.
///
/// # Returns
/// `Result<()>` after the column, migration record, and SQLite version commit in
/// one `BEGIN IMMEDIATE` transaction. Existing authority and epoch are unchanged.
///
/// # Errors
/// Refuses a source other than schema 1 and propagates SQL/commit failures. Marker
/// replacement and recovery journaling belong to the explicit migration workflow.
fn apply_schema2(connection: &mut Connection) -> Result<()> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 1,
        });
    }
    transaction.execute_batch(WATERMARK_MIGRATION)?;
    transaction.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (2, ?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![WATERMARK_MIGRATION_NAME, watermark_migration_checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 2)?;
    transaction.commit()?;
    Ok(())
}

/// Apply the schema-3 job ledger while preserving the previous change position.
///
/// `connection` is a mutable `&mut Connection` under exclusive maintenance access.
/// Returns `Result<()>` after all SQL, its checksum, and version commit together.
/// Refuses any source except schema 2; an error rolls back the transaction.
fn apply_schema3(connection: &mut Connection) -> Result<()> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 2 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 2,
        });
    }
    for statement in schema3::statements() {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (3, ?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![schema3::MIGRATION_NAME, schema3::checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 3)?;
    transaction.commit()?;
    Ok(())
}

/// Fingerprint the exact schema-2 SQL and separator saved in its migration record.
///
/// # Returns
/// An owned `String` containing the tagged BLAKE3 checksum of the migration.
fn watermark_migration_checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(WATERMARK_MIGRATION.as_bytes());
    hasher.update(b";\n");
    format!("b3:{}", hasher.finalize().to_hex())
}

/// Hash the exact initial schema statements and separators used by this migration.
///
/// # Returns
///
/// An owned `String` containing `b3:` followed by the lowercase BLAKE3 digest.
fn migration_checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MIGRATIONS_TABLE.as_bytes());
    hasher.update(b";\n");
    hasher.update(STATE_TABLE.as_bytes());
    hasher.update(b";\n");
    format!("b3:{}", hasher.finalize().to_hex())
}

/// Encode a filesystem path as a SQLite URI with read-only, immutable open flags.
///
/// # Arguments
///
/// * `path` — `&Path` whose Unix bytes are percent-encoded where necessary.
///
/// # Returns
///
/// An owned `String` beginning with `file:` and ending in `?mode=ro&immutable=1`.
/// Reserved characters in the path cannot become additional URI parameters.
fn readonly_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            // Formatting into a String cannot fail.
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
}

/// Resolve an initialization target into its existing parent and final entry name.
///
/// # Arguments
///
/// * `root` — requested `&Path`, either relative or absolute.
///
/// # Returns
///
/// `Result<(PathBuf, OsString)>` containing the canonical parent directory and owned
/// final name; `OsString` is written as `std::ffi::OsString` in the signature.
///
/// # Errors
///
/// Rejects missing/invalid final names or a parent that cannot be resolved or verified
/// as a directory. It does not create the parent or target.
fn destination(root: &Path) -> Result<(PathBuf, std::ffi::OsString)> {
    let name = root.file_name().ok_or_else(|| StorageError::InvalidPath {
        path: root.to_path_buf(),
        reason: "select a new named child of an existing directory",
    })?;
    if name == "." || name == ".." {
        return Err(StorageError::InvalidPath {
            path: root.to_path_buf(),
            reason: "root name must be a normal path component",
        });
    }
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent =
        fs::canonicalize(parent).map_err(|source| io("resolve existing parent", parent, source))?;
    require_directory(&parent)?;
    Ok((parent, name.to_os_string()))
}

/// Require an initialization target to have no existing filesystem entry.
///
/// # Arguments
///
/// * `path` — `&Path` of the proposed final root.
///
/// # Returns
///
/// `Result<()>` with `Ok(())` only when the entry is absent.
///
/// # Errors
///
/// Returns `ExistingTarget` for any existing entry, including dangling symlinks,
/// and an I/O error if the entry cannot be inspected. Publication separately uses
/// an atomic no-replace rename to handle races after this preliminary check.
fn reject_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(StorageError::ExistingTarget(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io("inspect initialization target", path, error)),
    }
}

/// Verify that a path names a directory rather than a symlink or another entry type.
///
/// # Arguments
///
/// * `path` — `&Path` inspected without following its final symlink.
///
/// # Returns
///
/// `Result<()>` with `Ok(())` when the observed entry is a non-symlink directory.
///
/// # Errors
///
/// Returns an I/O error for failed metadata access or `InvalidPath` for a wrong type.
fn require_directory(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io("inspect directory", path, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StorageError::InvalidPath {
            path: path.to_path_buf(),
            reason: "expected a non-symlink directory",
        });
    }
    Ok(())
}

/// Open a directory handle without following a final symlink or inheriting it on exec.
///
/// # Arguments
///
/// * `path` — `&Path` of the directory to open read-only.
///
/// # Returns
///
/// `Result<File>` containing the owned directory handle used for syncing or renaming.
///
/// # Errors
///
/// Returns a contextual I/O error if the directory cannot be opened with the required
/// flags.
fn open_directory(path: &Path) -> Result<File> {
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io("open directory without following links", path, error.into()))?;
    Ok(File::from(descriptor))
}

/// Check a regular non-symlink file and open it read-only without following links.
///
/// # Arguments
///
/// * `path` — `&Path` of the existing file to inspect.
///
/// # Returns
///
/// `Result<File>` containing an owned read-only handle verified as a regular file.
///
/// # Errors
///
/// Rejects non-regular or symlink entries and propagates metadata/open failures.
fn open_regular(path: &Path) -> Result<File> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io("inspect regular file", path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StorageError::InvalidPath {
            path: path.to_path_buf(),
            reason: "expected a regular non-symlink file",
        });
    }
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        io(
            "open regular file without following links",
            path,
            error.into(),
        )
    })?;
    let file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|source| io("verify opened file", path, source))?
        .is_file()
    {
        return Err(StorageError::InvalidPath {
            path: path.to_path_buf(),
            reason: "opened path is not a regular file",
        });
    }
    Ok(file)
}

/// Open a directory and request synchronization of its metadata to storage.
///
/// # Arguments
///
/// * `path` — `&Path` of the directory whose entries need synchronization.
///
/// # Returns
///
/// `Result<()>` with `Ok(())` after the directory handle's `sync_all` succeeds.
///
/// # Errors
///
/// Propagates contextual errors from opening or synchronizing the directory.
fn sync_directory(path: &Path) -> Result<()> {
    open_directory(path)?
        .sync_all()
        .map_err(|source| io("synchronize directory", path, source))
}

/// Attach an operation and owned path to an underlying filesystem error.
///
/// # Arguments
///
/// * `action` — `&'static str` describing the failed operation.
/// * `path` — `&Path` copied into the returned error for later diagnosis.
/// * `source` — owned `std::io::Error` retained as the underlying cause.
///
/// # Returns
///
/// A `StorageError::Io` value; this helper constructs an error without performing I/O.
fn io(action: &'static str, path: &Path, source: std::io::Error) -> StorageError {
    StorageError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}
