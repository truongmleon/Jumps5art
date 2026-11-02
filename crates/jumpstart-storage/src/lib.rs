//! Creates and checks the local folder that identifies a Jumpstart server.
//!
//! This folder holds bookkeeping data: the server ID, database layout versions,
//! and a label that future apps can use to detect a restored or replaced server.
//! These values appear in `format.json` and a SQLite database and must agree.
//! Storing actual song/photo contents is separate work.
//!
//! `initialize` prepares a new folder; `inspect` checks an existing one without repair.
//! Keeping these operations separate prevents a diagnostic read from creating a
//! replacement database when the expected one is missing or unreadable.
//!
//! `create_snapshot` makes a separate verified copy of this bookkeeping database;
//! `verify_snapshot` checks that copy even when its original source is unavailable.
//! This development snapshot excludes media, Worlds, and secrets, and is not a
//! complete server backup or a way to restore an installation.
//!
//! `inspect_capacity` observes how much space an existing directory's filesystem
//! reports as available to ordinary users. For example, it can explain why a
//! future import should wait for storage. It reports the default warning and
//! safety-reserve thresholds without registering a volume or accepting writes.
//!
//! Initialization publishes a fully verified sibling staging directory using an
//! atomic no-replace rename. Existing targets, including empty directories, are
//! never adopted. Failed staging is retained for diagnosis rather than removed.
//!
//! Inspection is deliberately offline: every future writer must hold an
//! exclusive lock on `metadata.lock` for its complete connection lifetime.
//! Inspection takes a shared lock, rejects uncheckpointed WAL/journal data, and
//! opens SQLite with `mode=ro&immutable=1`. It never creates journal sidecars or
//! repairs a database. Linux and macOS implement the filesystem boundary; other
//! platforms return an explicit unsupported error.

use std::path::{Path, PathBuf};

use jumpstart_domain::{BlobId, Counter, ServerId, SyncEpoch};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod capacity;

pub use capacity::{
    CapacityAssessment, CapacityReport, CapacityScope, assess_capacity, inspect_capacity,
};

pub const FORMAT_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 1;
pub const APPLICATION_ID: i32 = 0x4a53_4d44;
pub const FORMAT_FILE: &str = "format.json";
pub const LOCK_FILE: &str = "metadata.lock";
pub const DATABASE_DIRECTORY: &str = "database";
pub const DATABASE_FILE: &str = "jumpstart.sqlite";
/// Current version of the separate bootstrap metadata snapshot format.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
/// JSON manifest naming a verified local metadata snapshot's contents.
pub const SNAPSHOT_MANIFEST_FILE: &str = "snapshot.json";
/// Standalone SQLite database stored inside a metadata snapshot directory.
pub const SNAPSHOT_DATABASE_FILE: &str = "metadata.sqlite";

/// The answer to "which server bookkeeping root did this operation check?"
///
/// Initialization and inspection return its path, identity, versions, and database
/// check results together so the CLI can describe the checked installation. For
/// example, a matching folder name alone would not reveal that a different server's
/// database had been placed there. The report records successful checks; it does
/// not retain a database connection or lock, qualify a filesystem, or verify encryption.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MetadataReport {
    /// Absolute path of the verified metadata root.
    pub root: PathBuf,
    /// Persistent server authority read from both the marker and SQLite.
    pub server_id: ServerId,
    /// Persistent synchronization epoch shared by the marker and SQLite.
    pub sync_epoch: SyncEpoch,
    /// Supported version of the metadata directory format.
    pub format_version: u32,
    /// Verified SQLite schema version.
    pub schema_version: u32,
    /// Persistent SQLite header mode, not the inspection connection's mode.
    pub journal_mode: String,
    /// Successful SQLite integrity-check result, currently `"ok"`.
    pub integrity_check: String,
}

/// Labels which data a snapshot includes, so callers can explain what was protected.
///
/// For example, copying the initial bookkeeping database does not copy a music
/// library's audio files. The current `BootstrapMetadata` choice gives that limited
/// copy an explicit, machine-readable scope instead of relying on a folder name
/// or an ambiguous "backup succeeded" message. It covers server identity and the
/// initial database layout; media, Worlds, secrets, and app/device state are excluded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    /// Initial server bookkeeping before sequenced library changes exist.
    BootstrapMetadata,
}

/// The answer to "which snapshot was checked, and what database did it capture?"
///
/// Creation and verification return the bundle's path, source identity, versions,
/// scope, byte count, and fingerprint together. This lets a caller identify a copy
/// after moving it or losing its original source, rather than receiving only a
/// success flag. The saved `SnapshotManifest` supplies the comparison data on disk;
/// this report describes the operation's result at its current local path.
///
/// Size and fingerprint checks detect changed artifact bytes against the manifest;
/// they do not authenticate an untrusted supplier. Constructing a report does not
/// run verification. No restore or activation operation is provided.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotReport {
    /// Absolute path of the verified snapshot directory.
    pub root: PathBuf,
    /// Version of the snapshot manifest and artifact layout.
    pub snapshot_format_version: u32,
    /// Explicit limit on the data included in this artifact.
    pub scope: SnapshotScope,
    /// Server identity captured from the source database and format marker.
    pub server_id: ServerId,
    /// Captured label identifying the source's synchronization history.
    pub sync_epoch: SyncEpoch,
    /// Source metadata-directory format version.
    pub format_version: u32,
    /// Source SQLite schema version, checked again inside the copy.
    pub schema_version: u32,
    /// Captured change sequence, unavailable (`None`) in the bootstrap schema.
    ///
    /// Schema 1 has no stored change sequence. This snapshot cannot be used as a
    /// synchronization starting point; a later schema must capture its real value.
    pub change_seq: Option<Counter>,
    /// Exact byte length of the standalone SQLite artifact.
    pub database_bytes: Counter,
    /// BLAKE3 fingerprint of the verified standalone SQLite artifact.
    pub database_hash: BlobId,
}

/// On-disk JSON marker containing the format versions and server identity.
///
/// Its version and identity fields must agree with the SQLite database before
/// inspection can return a [`MetadataReport`]. Unknown JSON fields are rejected.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FormatMarker {
    format_version: u32,
    schema_version: u32,
    server_id: ServerId,
    sync_epoch: SyncEpoch,
}

/// Failure to create or verify a metadata root, including paths and causes.
///
/// `StagingRetained` identifies incomplete work left for diagnosis. An error
/// after publication can leave a complete root at the requested destination.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error(
        "metadata target already exists: {0}; select a new path (even an empty existing directory is not adopted)"
    )]
    ExistingTarget(PathBuf),
    #[error("invalid metadata path {path}: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("{action} failed for {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid format marker: {0}")]
    InvalidMarker(#[from] serde_json::Error),
    #[error("unsupported metadata format {found}; this binary supports {supported}")]
    UnsupportedFormat { found: u32, supported: u32 },
    #[error("unsupported metadata schema {found}; this binary supports {supported}")]
    UnsupportedSchema { found: i64, supported: u32 },
    #[error("SQLite metadata verification failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("metadata integrity check failed: {0}")]
    Integrity(&'static str),
    #[error("metadata identity does not agree between format.json and SQLite")]
    IdentityMismatch,
    #[error("metadata migration record or schema definition does not match this binary")]
    MigrationMismatch,
    #[error("metadata is in use by an exclusive writer; stop it before offline inspection")]
    InUse,
    #[error("metadata needs recovery before offline inspection: {0}")]
    NeedsRecovery(&'static str),
    #[error("metadata operation did not complete; staging is retained at {staging}: {source}")]
    StagingRetained {
        staging: PathBuf,
        #[source]
        source: Box<StorageError>,
    },
    /// The snapshot manifest or database does not satisfy the supported contract.
    #[error("invalid metadata snapshot: {0}")]
    InvalidSnapshot(&'static str),
    /// Snapshot manifest JSON could not be decoded into its required shape.
    #[error("invalid metadata snapshot manifest: {0}")]
    InvalidSnapshotManifest(#[source] serde_json::Error),
    /// Snapshot layout comes from a different, unsupported format version.
    #[error("unsupported metadata snapshot format {found}; this binary supports {supported}")]
    UnsupportedSnapshotFormat { found: u32, supported: u32 },
    /// Snapshot creation could not finish within its bounded copy workflow.
    #[error("metadata snapshot could not be completed: {0}")]
    SnapshotUnavailable(&'static str),
    /// Filesystem measurements cannot produce a valid bounded capacity report.
    #[error("storage capacity observation is invalid: {0}")]
    InvalidCapacity(&'static str),
    #[error("storage operations are currently supported only on Linux and macOS")]
    UnsupportedPlatform,
}

/// Result of a storage operation: a value of type `T` or a [`StorageError`].
pub type Result<T> = std::result::Result<T, StorageError>;

/// Create a new metadata root without adopting or overwriting any existing path.
/// The parent directory must already exist. A failed operation can leave an
/// explicitly identified staging directory; it never deletes an existing root.
///
/// # Arguments
///
/// * `root` — borrowed `&Path` naming a new child of an existing directory.
///
/// # Returns
///
/// `Result<MetadataReport>` containing the verified, published root and identity.
///
/// # Errors
///
/// Returns a [`StorageError`] for existing targets, invalid paths, failed I/O or
/// verification, or unsupported platforms. A failure after publication does not
/// remove the published root; inspect it before retrying initialization.
pub fn initialize(root: &Path) -> Result<MetadataReport> {
    platform::initialize(root)
}

/// Inspect a checkpointed metadata root without changing any files within it.
/// Refuses an active writer and nonempty WAL/rollback journals; does not recover.
///
/// # Arguments
///
/// * `root` — borrowed `&Path` of an existing, offline metadata root.
///
/// # Returns
///
/// `Result<MetadataReport>` containing the verified identity, versions, and path.
///
/// # Errors
///
/// Returns a [`StorageError`] for missing or invalid files, incompatible versions,
/// inconsistent identities or schema, active writers, pending recovery, failed
/// I/O, or unsupported platforms. It does not repair the root on failure.
pub fn inspect(root: &Path) -> Result<MetadataReport> {
    platform::inspect(root)
}

/// Create a verified standalone copy of an offline bootstrap metadata database.
///
/// # Arguments
///
/// * `root` — `&Path` of the existing, checkpointed metadata root.
/// * `output` — `&Path` naming a new directory outside `root`, under an existing parent.
///
/// # Returns
///
/// `Result<SnapshotReport>` describing the published copy's identity and contents.
/// The source remains unchanged and is locked against cooperating writers during capture.
///
/// # Errors
///
/// Rejects invalid or active sources, pending recovery, overlapping paths, existing
/// destinations, unsupported platforms, and failed copying or verification. Failed
/// staging is retained for diagnosis. An error after publication can leave the
/// verified destination in place; verify it before retrying.
pub fn create_snapshot(root: &Path, output: &Path) -> Result<SnapshotReport> {
    platform::create_snapshot(root, output)
}

/// Verify a bootstrap metadata snapshot without its original source or file changes.
///
/// # Arguments
///
/// * `root` — `&Path` of the snapshot directory containing its manifest and database.
///
/// # Returns
///
/// `Result<SnapshotReport>` after checking the manifest, database size/fingerprint,
/// schema, integrity, and captured identities. This does not restore the snapshot.
///
/// # Errors
///
/// Rejects missing, corrupt, inconsistent, or unsupported snapshots, unexpected
/// contents, unsafe paths, and unsupported platforms. It does not repair failures.
pub fn verify_snapshot(root: &Path) -> Result<SnapshotReport> {
    platform::verify_snapshot(root)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{MetadataReport, Path, Result, SnapshotReport, StorageError};

    /// Refuse initialization on a platform without the filesystem implementation.
    ///
    /// # Arguments
    ///
    /// * `_root` — requested `&Path`; unused because no filesystem work is done.
    ///
    /// # Returns
    ///
    /// `Result<MetadataReport>`, always `Err(StorageError::UnsupportedPlatform)`.
    pub fn initialize(_root: &Path) -> Result<MetadataReport> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse inspection on a platform without the filesystem implementation.
    ///
    /// # Arguments
    ///
    /// * `_root` — requested `&Path`; unused because no filesystem work is done.
    ///
    /// # Returns
    ///
    /// `Result<MetadataReport>`, always `Err(StorageError::UnsupportedPlatform)`.
    pub fn inspect(_root: &Path) -> Result<MetadataReport> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse snapshot creation before filesystem work on unsupported platforms.
    ///
    /// # Arguments
    ///
    /// * `_root` — unused source `&Path`.
    /// * `_output` — unused destination `&Path`.
    ///
    /// # Returns
    ///
    /// `Result<SnapshotReport>`, always `Err(StorageError::UnsupportedPlatform)`.
    pub fn create_snapshot(_root: &Path, _output: &Path) -> Result<SnapshotReport> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse snapshot verification before filesystem work on unsupported platforms.
    ///
    /// # Arguments
    ///
    /// * `_root` — unused snapshot `&Path`.
    ///
    /// # Returns
    ///
    /// `Result<SnapshotReport>`, always `Err(StorageError::UnsupportedPlatform)`.
    pub fn verify_snapshot(_root: &Path) -> Result<SnapshotReport> {
        Err(StorageError::UnsupportedPlatform)
    }
}
