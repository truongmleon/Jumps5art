//! Linux and macOS filesystem implementation of metadata initialization and inspection.
//! Prepares and verifies SQLite metadata in staging, publishes without replacement,
//! and checks offline roots under the maintenance lock.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use jumpstart_domain::{ServerId, SyncEpoch};
use rusqlite::{Connection, OpenFlags, params};
use rustix::fs::{Mode, OFlags, RenameFlags};
use uuid::Uuid;

use super::{
    APPLICATION_ID, DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, FORMAT_VERSION, FormatMarker,
    LOCK_FILE, MetadataReport, Result, SCHEMA_VERSION, StorageError,
};

const MIGRATION_NAME: &str = "initial_metadata";
const MIGRATIONS_TABLE: &str = "CREATE TABLE schema_migrations (\n    version INTEGER PRIMARY KEY CHECK (version > 0),\n    name TEXT NOT NULL,\n    checksum TEXT NOT NULL,\n    applied_at TEXT NOT NULL\n) STRICT";
const STATE_TABLE: &str = "CREATE TABLE server_state (\n    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n    format_version INTEGER NOT NULL CHECK (format_version > 0),\n    server_id BLOB NOT NULL CHECK (length(server_id) = 16),\n    sync_epoch BLOB NOT NULL CHECK (length(sync_epoch) = 16)\n) STRICT";
const MARKER_LIMIT: u64 = 64 * 1024;

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
    // Drop the exclusive lock before exercising exactly the same verification
    // path as subsequent offline inspection.
    drop(lock);
    inspect(staging)
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
        params![SCHEMA_VERSION, MIGRATION_NAME, migration_checksum()],
    )?;
    transaction.execute(
        "INSERT INTO server_state (singleton, format_version, server_id, sync_epoch) VALUES (1, ?1, ?2, ?3)",
        params![FORMAT_VERSION, marker.server_id.as_uuid().as_bytes().as_slice(), marker.sync_epoch.as_uuid().as_bytes().as_slice()],
    )?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
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
    require_directory(root)?;
    let root =
        fs::canonicalize(root).map_err(|source| io("resolve metadata root", root, source))?;
    let database_directory = root.join(DATABASE_DIRECTORY);
    require_directory(&database_directory)?;
    let lock_path = root.join(LOCK_FILE);
    let lock = open_regular(&lock_path)?;
    FileExt::try_lock_shared(&lock).map_err(|source| {
        if source.kind() == std::io::ErrorKind::WouldBlock {
            StorageError::InUse
        } else {
            io("acquire offline inspection lock", &lock_path, source)
        }
    })?;
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
    if marker.schema_version != SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema {
            found: marker.schema_version.into(),
            supported: SCHEMA_VERSION,
        });
    }
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
    verify_database(&connection, &marker)?;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(MetadataReport {
        root,
        server_id: marker.server_id,
        sync_epoch: marker.sync_epoch,
        format_version: marker.format_version,
        schema_version: marker.schema_version,
        journal_mode: "wal".to_owned(),
        integrity_check: "ok".to_owned(),
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
    if version != i64::from(SCHEMA_VERSION) {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
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
    for (name, expected) in [
        ("schema_migrations", MIGRATIONS_TABLE),
        ("server_state", STATE_TABLE),
    ] {
        let actual: String = connection.query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )?;
        if actual.trim() != expected.trim() {
            return Err(StorageError::MigrationMismatch);
        }
    }
    let schema_objects: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if schema_objects != 2 {
        return Err(StorageError::MigrationMismatch);
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
    if migrations
        != [(
            SCHEMA_VERSION.into(),
            MIGRATION_NAME.to_owned(),
            migration_checksum(),
        )]
    {
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
    Ok(())
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
