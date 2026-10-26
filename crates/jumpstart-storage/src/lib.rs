//! Initial metadata-root creation and offline, non-mutating inspection.
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

use jumpstart_domain::{ServerId, SyncEpoch};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const FORMAT_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 1;
pub const APPLICATION_ID: i32 = 0x4a53_4d44;
pub const FORMAT_FILE: &str = "format.json";
pub const LOCK_FILE: &str = "metadata.lock";
pub const DATABASE_DIRECTORY: &str = "database";
pub const DATABASE_FILE: &str = "jumpstart.sqlite";

/// Verified metadata identity and on-disk format. No filesystem qualification or
/// encryption guarantee is implied by successful initialization or inspection.
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
    #[error("initialization did not complete; staging is retained at {staging}: {source}")]
    StagingRetained {
        staging: PathBuf,
        #[source]
        source: Box<StorageError>,
    },
    #[error("metadata storage is currently supported only on Linux and macOS")]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{MetadataReport, Path, Result, StorageError};

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
}
