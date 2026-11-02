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
//! `migrate` upgrades an existing root after saving a verified snapshot. A retained
//! journal bridges the database commit and the separate format-marker replacement,
//! so an interrupted upgrade can be finished explicitly instead of guessed at by
//! inspection. Schema 2 stores a change position for future mutation bookkeeping.
//!
//! Schema 3 also saves local snapshot jobs and their changes. For example, a queued
//! copy remains visible after the CLI exits, and a finished copy has a recorded
//! outcome. The synchronous runner keeps exclusive offline access while it works;
//! it is not a daemon, parallel worker pool, or automatic retry scheduler.
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

// Reuse the frozen schema-1 fixture from both unit and integration tests.
#[cfg(test)]
extern crate self as jumpstart_storage;

use std::path::{Path, PathBuf};

use jumpstart_domain::{
    BlobId, Counter, JobId, JobState, JobTransitionError, PositiveCounter, ServerId, SyncEpoch,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const FORMAT_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 3;
pub const APPLICATION_ID: i32 = 0x4a53_4d44;
pub const FORMAT_FILE: &str = "format.json";
pub const LOCK_FILE: &str = "metadata.lock";
pub const DATABASE_DIRECTORY: &str = "database";
pub const DATABASE_FILE: &str = "jumpstart.sqlite";
/// Journal retained until an explicit metadata upgrade finishes durably.
pub const MIGRATION_FILE: &str = "migration.json";
/// Current version of the standalone metadata snapshot manifest format.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
/// JSON manifest naming a verified local metadata snapshot's contents.
pub const SNAPSHOT_MANIFEST_FILE: &str = "snapshot.json";
/// Standalone SQLite database stored inside a metadata snapshot directory.
pub const SNAPSHOT_DATABASE_FILE: &str = "metadata.sqlite";

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
    /// Stored change high-water mark, unavailable in schema 1 and initially zero in 2.
    ///
    /// Schema 2 records this value explicitly; schema 3 advances it with saved job
    /// changes. Application RPCs and a device synchronization service remain future work.
    pub change_seq: Option<Counter>,
    /// Persistent SQLite header mode, not the inspection connection's mode.
    pub journal_mode: String,
    /// Successful SQLite integrity-check result, currently `"ok"`.
    pub integrity_check: String,
}

/// The data covered by a snapshot, so a metadata copy cannot imply a full backup.
///
/// Schemas 1/2 contain initial server bookkeeping. Schema 3 also includes saved
/// snapshot jobs and their changes. Both scopes exclude media, Worlds, and secrets.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    /// Initial server bookkeeping before sequenced library changes exist.
    BootstrapMetadata,
    /// Server bookkeeping including durable local jobs and their recorded changes.
    /// Media files, secrets, and Worlds remain outside this snapshot's coverage.
    Metadata,
}

/// Verified identity and contents of one standalone metadata snapshot.
///
/// The database's size and fingerprint detect accidental changes to the artifact;
/// they do not authenticate a snapshot supplied by an untrusted person. Verification
/// works without the source root, but no restore or activation operation is provided.
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
    /// Captured stored change position, unavailable (`None`) in schema 1.
    ///
    /// Schema 2 captures the actual database value, initially zero. No change-feed
    /// service or synchronization starting-point workflow is implemented yet.
    pub change_seq: Option<Counter>,
    /// Exact byte length of the standalone SQLite artifact.
    pub database_bytes: Counter,
    /// BLAKE3 fingerprint of the verified standalone SQLite artifact.
    pub database_hash: BlobId,
}

/// Describes an explicit metadata upgrade and where its prior state was preserved.
///
/// For example, upgrading a scratch schema-1 root first saves a verified snapshot,
/// then updates the database and format marker. Callers need both the checked new
/// metadata and the old snapshot's location to understand that result. This report
/// also distinguishes a resumed upgrade from a clean root that needed no change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MigrationReport {
    /// Checked metadata after the upgrade or already-current no-op.
    pub metadata: MetadataReport,
    /// Schema before this upgrade, or the current schema for a no-op.
    pub from_schema_version: u32,
    /// Target of the recorded workflow; a legacy journal may finish at schema 2.
    pub to_schema_version: u32,
    /// Independently verified pre-upgrade snapshot; absent for a clean no-op.
    pub backup: Option<SnapshotReport>,
    /// Whether a previously published migration journal was resumed.
    pub resumed: bool,
}

/// A saved request to copy metadata, together with its current recorded outcome.
///
/// For example, enqueueing a snapshot records its destination before execution,
/// so another CLI invocation can find it later. The state/revision/worker token
/// are checked against committed change records; successful output is recorded
/// only after actual publication and verification. This value describes the job;
/// constructing or deserializing it does not start work or prove an artifact valid.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotJob {
    /// Stable identity of this queued user request.
    pub job_id: JobId,
    /// Absolute UTF-8 destination selected before the worker starts.
    pub output_path: PathBuf,
    /// Current recorded stage; this runner supports a closed snapshot workflow.
    pub state: JobState,
    /// Positive revision used to reject outdated control requests.
    pub revision: PositiveCounter,
    /// Worker token counter, advanced when starting or fencing an attempt.
    pub attempt_generation: Counter,
    /// Change position that created this job, used for stable listing order.
    pub created_seq: PositiveCounter,
    /// Informational server UTC milliseconds at creation; not a conflict clock.
    pub created_at_ms: i64,
    /// Informational server UTC milliseconds at the latest recorded change.
    /// Wall-clock corrections can make this earlier than a prior observation.
    pub updated_at_ms: i64,
    /// Verified snapshot result, present only after successful completion.
    pub result: Option<SnapshotReport>,
    /// Bounded actionable failure explanation, present only for failed work.
    pub error: Option<String>,
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
    /// The retained journal or its recovery snapshot disagrees with this upgrade.
    #[error("invalid metadata migration: {0}")]
    InvalidMigration(&'static str),
    /// A migration journal is not strict, bounded JSON of the supported shape.
    #[error("invalid metadata migration journal: {0}")]
    InvalidMigrationJournal(#[source] serde_json::Error),
    /// Older readable metadata must be explicitly upgraded before storing jobs.
    #[error(
        "snapshot jobs require metadata schema 3; found {found}; migrate the offline root first"
    )]
    JobsUnavailable { found: u32 },
    /// No saved request has the supplied job identity.
    #[error("snapshot job does not exist: {0}")]
    JobNotFound(JobId),
    /// Saved job data or a requested local job action violates this workflow.
    #[error("invalid snapshot job: {0}")]
    InvalidJob(&'static str),
    /// The shared job policy rejected a stale or illegal state change.
    #[error("job transition rejected: {0}")]
    JobTransition(#[from] JobTransitionError),
    /// Another job change would exceed the supported global change counter.
    #[error("metadata change sequence is exhausted")]
    ChangeSequenceExhausted,
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

/// Create a verified standalone copy of an offline metadata database.
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

/// Verify a metadata snapshot without its original source or file changes.
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

/// Upgrade offline metadata after saving a verified copy of its previous database.
///
/// # Arguments
///
/// * `root` — `&Path` of an existing metadata root; this operation holds its
///   exclusive maintenance lock until all database and marker work is finished.
/// * `snapshot_output` — `&Path` for a new pre-upgrade snapshot outside `root`.
///   Resume uses the same path recorded in the retained migration journal.
///
/// # Returns
///
/// `Result<MigrationReport>` with checked metadata and its recovery snapshot.
/// A clean, current root is a no-op and does not create `snapshot_output`.
///
/// # Errors
///
/// Refuses active readers/writers, invalid roots or recovery artifacts, unsupported
/// schemas, existing first-use snapshot outputs, and unsafe paths. Failures after
/// journal publication retain it; rerun this operation with the same paths to
/// finish. Inspection never performs this recovery implicitly. An error after
/// journal removal can leave a completed upgrade; inspect before retrying.
pub fn migrate(root: &Path, snapshot_output: &Path) -> Result<MigrationReport> {
    platform::migrate(root, snapshot_output)
}

/// Save a queued metadata-copy request without starting the copy.
///
/// `root: &Path` selects existing schema-3 metadata; `output: &Path` names a new
/// UTF-8 destination outside it. Returns `Result<SnapshotJob>` after the request
/// and its full change projection commit together. Invalid paths, busy storage,
/// incompatible schema, and database errors leave no accepted request.
pub fn enqueue_snapshot_job(root: &Path, output: &Path) -> Result<SnapshotJob> {
    platform::enqueue_snapshot_job(root, output)
}

/// Execute one queued request synchronously and save its verified outcome.
///
/// `root: &Path` selects offline metadata and `job_id: JobId` selects the request.
/// Returns `Result<SnapshotJob>`; a copy failure is a saved `Failed` job with an
/// explanation, while an API/storage failure is `Err`. The caller must inspect
/// the returned state. Exclusive maintenance access covers the run, without a
/// database write transaction spanning the copy. No incomplete output is deleted.
pub fn run_snapshot_job(root: &Path, job_id: JobId) -> Result<SnapshotJob> {
    platform::run_snapshot_job(root, job_id)
}

/// Read one saved job after checking its committed state-change history.
///
/// `root: &Path` selects checkpointed metadata; `job_id: JobId` selects the record.
/// Returns `Result<SnapshotJob>` or a not-found, busy, recovery, or validation
/// error. Reading does not initialize storage, repair WAL, or execute the request.
pub fn inspect_job(root: &Path, job_id: JobId) -> Result<SnapshotJob> {
    platform::inspect_job(root, job_id)
}

/// Read a bounded page of the newest saved jobs, ordered by creation sequence.
///
/// `root: &Path` selects checkpointed metadata and `limit: u16` is in `1..=100`.
/// Returns `Result<Vec<SnapshotJob>>`; invalid limits or inconsistent stored jobs
/// fail instead of silently presenting a partial successful list.
pub fn list_jobs(root: &Path, limit: u16) -> Result<Vec<SnapshotJob>> {
    platform::list_jobs(root, limit)
}

/// Cancel queued work only if the caller observed its current revision.
///
/// `root: &Path` selects offline metadata, `job_id: JobId` selects the request,
/// and `expected_revision: PositiveCounter` detects an outdated control action.
/// Returns `Result<SnapshotJob>` after committing cancellation and its change
/// projection. Running and terminal work cannot be canceled through this API.
pub fn cancel_job(
    root: &Path,
    job_id: JobId,
    expected_revision: PositiveCounter,
) -> Result<SnapshotJob> {
    platform::cancel_job(root, job_id, expected_revision)
}

/// Explicitly reconcile a stopped command's WAL and incomplete active jobs.
///
/// `root: &Path` selects existing schema-3 metadata. Returns
/// `Result<Vec<SnapshotJob>>` with jobs changed from active to failed, fencing
/// their old tokens. Queued work remains queued. Recovery requires exclusive
/// access, validates committed WAL before writing, and never infers completion
/// from an existing output or deletes retained artifacts.
pub fn recover_jobs(root: &Path) -> Result<Vec<SnapshotJob>> {
    platform::recover_jobs(root)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{
        JobId, MetadataReport, MigrationReport, Path, PositiveCounter, Result, SnapshotJob,
        SnapshotReport, StorageError,
    };

    /// Refuse queue writes without touching `root` or `output` (`&Path`).
    /// Returns `Result<SnapshotJob>`, always the unsupported-platform error.
    pub fn enqueue_snapshot_job(_root: &Path, _output: &Path) -> Result<SnapshotJob> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse executing `job_id: JobId` at `root: &Path` on this platform.
    /// Returns `Result<SnapshotJob>`, always the unsupported-platform error.
    pub fn run_snapshot_job(_root: &Path, _job_id: JobId) -> Result<SnapshotJob> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse inspecting `job_id: JobId` at `root: &Path` without filesystem work.
    /// Returns `Result<SnapshotJob>`, always the unsupported-platform error.
    pub fn inspect_job(_root: &Path, _job_id: JobId) -> Result<SnapshotJob> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse listing `limit: u16` jobs at `root: &Path` without filesystem work.
    /// Returns `Result<Vec<SnapshotJob>>`, always the unsupported-platform error.
    pub fn list_jobs(_root: &Path, _limit: u16) -> Result<Vec<SnapshotJob>> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse canceling a job at a `PositiveCounter` revision on this platform.
    /// The path/ID are unused; `Result<SnapshotJob>` is always unsupported.
    pub fn cancel_job(
        _root: &Path,
        _job_id: JobId,
        _revision: PositiveCounter,
    ) -> Result<SnapshotJob> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse recovery at `root: &Path` without reading or changing files.
    /// Returns `Result<Vec<SnapshotJob>>`, always the unsupported-platform error.
    pub fn recover_jobs(_root: &Path) -> Result<Vec<SnapshotJob>> {
        Err(StorageError::UnsupportedPlatform)
    }

    /// Refuse upgrades without touching paths on unsupported storage platforms.
    ///
    /// `root` and `snapshot_output` are borrowed `&Path` requests. Returns
    /// `Result<MigrationReport>`, always `Err(StorageError::UnsupportedPlatform)`.
    pub fn migrate(_root: &Path, _snapshot_output: &Path) -> Result<MigrationReport> {
        Err(StorageError::UnsupportedPlatform)
    }

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
