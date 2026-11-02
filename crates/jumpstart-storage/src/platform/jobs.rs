//! Saves and runs local metadata-snapshot jobs without forgetting their outcomes.
//!
//! For example, an interrupted snapshot command must remain visible after its
//! process exits. Each accepted status change updates the job, its full change
//! projection, and the global change position in one short SQLite transaction.
//! History is replayed through JobLifecycle before a stored status is trusted.
//!
//! This is an offline maintenance preview. A synchronous run retains the root's
//! exclusive maintenance lock, but never a database write transaction, while
//! copying files. It provides no daemon, worker pool, timers, or live cancellation.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jumpstart_domain::{Counter, JobId, JobLifecycle, JobState, PositiveCounter};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use super::{
    VerifiedMetadata, destination, io, lock_metadata, open_regular, open_verified_metadata,
    read_change_seq, read_marker, readonly_uri, reject_existing, snapshot, sync_directory,
    verify_database, verify_locked_metadata,
};
use crate::{
    DATABASE_DIRECTORY, DATABASE_FILE, MIGRATION_FILE, MetadataReport, Result, SnapshotJob,
    SnapshotReport, SnapshotScope, StorageError,
};

const MAX_HISTORY_ROWS: usize = 16;
const MAX_PROJECTION_BYTES: usize = 65_536;
const MAX_ERROR_BYTES: usize = 4096;

/// Holds the checked writable connection and lock until its final checkpoint.
///
/// A job must not acknowledge a commit while its writer has already released the
/// maintenance lock. Keeping the database file handle and checked source together
/// also lets explicit completion synchronize the actual file before returning.
struct Writer {
    source: VerifiedMetadata,
    database: File,
}

/// Identifies real execution boundaries used by process-interruption tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunStep {
    RunningCommitted,
    SnapshotPublished,
    VerifyingCommitted,
    CompletedCommitted,
}

/// Accept a snapshot request durably without starting its file copy.
///
/// `root` is an existing schema-3 metadata `&Path`; `output` names a new directory
/// outside it. Returns `Result<SnapshotJob>` in Queued after commit/checkpoint.
/// Invalid destinations and storage failures return errors without adoption.
pub(crate) fn enqueue_snapshot_job(root: &Path, output: &Path) -> Result<SnapshotJob> {
    let mut writer = open_writer(root, false)?;
    let output_path = checked_output(&writer.source.report.root, output, true)?;
    let now = now_ms()?;
    let mut job = SnapshotJob {
        job_id: JobId::new(),
        output_path,
        state: JobState::Queued,
        revision: PositiveCounter::ONE,
        attempt_generation: Counter::ZERO,
        created_seq: PositiveCounter::ONE,
        created_at_ms: now,
        updated_at_ms: now,
        result: None,
        error: None,
    };
    persist_change(&mut writer.source, None, &mut job)?;
    finish_writer(writer)?;
    Ok(job)
}

/// Run one queued request synchronously and persist the accepted outcome.
///
/// `root` selects schema-3 metadata and `job_id` is its validated `JobId`.
/// Returns `Result<SnapshotJob>` after a verified completion or recorded failure.
/// A returned Failed record means work failed; callers must display that outcome.
/// Storage/ledger errors can leave an incomplete job for explicit recovery.
pub(crate) fn run_snapshot_job(root: &Path, job_id: JobId) -> Result<SnapshotJob> {
    run_with_hook(root, job_id, |_| Ok(()))
}

/// Execute the actual run with private milestone injection for crash tests.
///
/// `root` and `job_id` select the request; `milestone` observes `RunStep` values
/// and may stop execution. Returns the same `Result<SnapshotJob>` as the normal
/// runner. No environment-controlled hook is exposed by production commands.
fn run_with_hook(
    root: &Path,
    job_id: JobId,
    mut milestone: impl FnMut(RunStep) -> Result<()>,
) -> Result<SnapshotJob> {
    let mut writer = open_writer(root, false)?;
    let (mut job, mut lifecycle) = load_job(&writer.source.connection, job_id)?;
    if job.state != JobState::Queued {
        return Err(StorageError::InvalidJob(
            "only queued snapshot jobs can run",
        ));
    }
    transition(
        &mut writer.source,
        &mut job,
        &mut lifecycle,
        JobState::Running,
        None,
        None,
    )?;
    milestone(RunStep::RunningCommitted)?;

    let result = (|| {
        let output = checked_output(&writer.source.report.root, &job.output_path, false)?;
        if output != job.output_path {
            return Err(StorageError::InvalidJob(
                "snapshot destination path changed",
            ));
        }
        snapshot::create_snapshot_from_verified(&writer.source, &job.output_path)
    })();
    let captured = match result {
        Ok(report) => report,
        Err(error) => {
            transition(
                &mut writer.source,
                &mut job,
                &mut lifecycle,
                JobState::Failed,
                None,
                Some(bounded_error(&error)),
            )?;
            finish_writer(writer)?;
            return Ok(job);
        }
    };
    milestone(RunStep::SnapshotPublished)?;
    transition(
        &mut writer.source,
        &mut job,
        &mut lifecycle,
        JobState::Verifying,
        None,
        None,
    )?;
    milestone(RunStep::VerifyingCommitted)?;
    match snapshot::verify_snapshot(&job.output_path) {
        Ok(verified) if verified == captured => {
            transition(
                &mut writer.source,
                &mut job,
                &mut lifecycle,
                JobState::Completed,
                Some(verified),
                None,
            )?;
            milestone(RunStep::CompletedCommitted)?;
        }
        outcome => {
            let error = match outcome {
                Err(error) => bounded_error(&error),
                Ok(_) => "published snapshot no longer matches the accepted copy".to_owned(),
            };
            transition(
                &mut writer.source,
                &mut job,
                &mut lifecycle,
                JobState::Failed,
                None,
                Some(error),
            )?;
        }
    }
    finish_writer(writer)?;
    Ok(job)
}

/// Read one checked job and replay its bounded persisted transition history.
///
/// `root` is schema-3 metadata; `job_id` selects the record. Returns
/// `Result<SnapshotJob>` without changing files. A missing job or invalid history
/// is an explicit error rather than a synthesized empty result.
pub(crate) fn inspect_job(root: &Path, job_id: JobId) -> Result<SnapshotJob> {
    let source = open_verified_metadata(root)?;
    require_jobs(&source.report)?;
    let (job, _) = load_job(&source.connection, job_id)?;
    finish_reader(source)?;
    Ok(job)
}

/// List a bounded number of jobs in reverse durable acceptance order.
///
/// `root` is schema-3 metadata and `limit` must be `1..=100`. Returns
/// `Result<Vec<SnapshotJob>>`; order uses created change positions, not clocks.
/// Invalid limits are refused before accessing `root`.
pub(crate) fn list_jobs(root: &Path, limit: u16) -> Result<Vec<SnapshotJob>> {
    if !(1..=100).contains(&limit) {
        return Err(StorageError::InvalidJob("job list limit must be 1..=100"));
    }
    let source = open_verified_metadata(root)?;
    require_jobs(&source.report)?;
    let ids = job_ids(&source.connection, Some(limit))?;
    let jobs = ids
        .into_iter()
        .map(|id| load_job(&source.connection, id).map(|v| v.0))
        .collect::<Result<Vec<_>>>()?;
    finish_reader(source)?;
    Ok(jobs)
}

/// List one recorded state before applying the bounded result limit.
///
/// For example, newer completed work must not hide older queued requests when a
/// caller asks for the first waiting jobs. The state/acceptance index narrows the
/// SQL selection; each returned record still receives normal history validation.
///
/// `root` is schema-3 metadata, `state` is one of this runner's six states, and
/// `limit` must be `1..=100`. Returns `Result<Vec<SnapshotJob>>` in reverse durable
/// acceptance order. Invalid states and limits fail before filesystem access.
pub(crate) fn list_jobs_in_state(
    root: &Path,
    state: JobState,
    limit: u16,
) -> Result<Vec<SnapshotJob>> {
    if !(1..=100).contains(&limit) {
        return Err(StorageError::InvalidJob("job list limit must be 1..=100"));
    }
    let state = state_name(state)?;
    let source = open_verified_metadata(root)?;
    require_jobs(&source.report)?;
    let ids = {
        let mut statement = source
            .connection
            .prepare("SELECT job_id FROM jobs WHERE state=?1 ORDER BY created_seq DESC LIMIT ?2")?;
        let mut rows = statement.query(params![state, i64::from(limit)])?;
        let mut ids = Vec::with_capacity(usize::from(limit));
        while let Some(row) = rows.next()? {
            ids.push(parse_id(&row.get::<_, Vec<u8>>(0)?)?);
        }
        ids
    };
    let jobs = ids
        .into_iter()
        .map(|id| load_job(&source.connection, id).map(|value| value.0))
        .collect::<Result<Vec<_>>>()?;
    finish_reader(source)?;
    Ok(jobs)
}

/// Cancel an unstarted job using the revision the caller actually observed.
///
/// `root` and `job_id` select the request; `expected_revision` is its
/// `PositiveCounter` revision. Returns the durably Canceled `SnapshotJob`.
/// Running and terminal jobs are refused; no output or staging files are removed.
pub(crate) fn cancel_job(
    root: &Path,
    job_id: JobId,
    expected_revision: PositiveCounter,
) -> Result<SnapshotJob> {
    let mut writer = open_writer(root, false)?;
    let (mut job, mut lifecycle) = load_job(&writer.source.connection, job_id)?;
    if expected_revision != job.revision {
        return Err(jumpstart_domain::JobTransitionError::RevisionMismatch {
            expected: expected_revision,
            actual: job.revision,
        }
        .into());
    }
    if job.state != JobState::Queued {
        return Err(StorageError::InvalidJob(
            "only queued snapshot jobs can be canceled",
        ));
    }
    transition(
        &mut writer.source,
        &mut job,
        &mut lifecycle,
        JobState::Canceled,
        None,
        None,
    )?;
    finish_writer(writer)?;
    Ok(job)
}

/// Fence interrupted active runs and checkpoint already committed job decisions.
///
/// `root` is schema-3 metadata. Returns `Result<Vec<SnapshotJob>>` containing only
/// Running/Verifying records changed to Failed. Queued and terminal jobs remain
/// unchanged. Existing output is retained and never treated as proof of completion.
/// The exclusive maintenance lock refuses recovery alongside an active runner.
pub(crate) fn recover_jobs(root: &Path) -> Result<Vec<SnapshotJob>> {
    let mut writer = open_writer(root, true)?;
    let ids = job_ids(&writer.source.connection, None)?;
    let mut recovered = Vec::new();
    for id in ids {
        let (mut job, mut lifecycle) = load_job(&writer.source.connection, id)?;
        if matches!(job.state, JobState::Running | JobState::Verifying) {
            transition(&mut writer.source, &mut job, &mut lifecycle, JobState::Failed, None,
                Some("snapshot run was interrupted; retained output requires separate inspection before retry".to_owned()))?;
            recovered.push(job);
        }
    }
    finish_writer(writer)?;
    Ok(recovered)
}

/// Open a schema-3 writer only after a checked read-only view has passed validation.
///
/// `root` selects metadata and `recovery` permits committed WAL from a dead process.
/// Returns `Result<Writer>` with continuous exclusive maintenance ownership.
/// Pending migrations, unsupported schemas, unsafe sidecars, and corrupt ledgers
/// are refused before the writable connection opens.
fn open_writer(root: &Path, recovery: bool) -> Result<Writer> {
    let (root, lock) = lock_metadata(root, true)?;
    let source = if recovery {
        recovery_reader(root, lock)?
    } else {
        verify_locked_metadata(root, lock)?
    };
    require_jobs(&source.report)?;
    verify_all_jobs(&source.connection)?;
    let VerifiedMetadata {
        report,
        marker,
        connection,
        _lock,
    } = source;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    checked_sidecars(&report.root, recovery)?;
    let path = report.root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
    let database = open_regular(&path)?;
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    if !connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)? {
        return Err(StorageError::InvalidJob(
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
        return Err(StorageError::InvalidJob("job writer must retain WAL mode"));
    }
    verify_database(&connection, &marker)?;
    verify_all_jobs(&connection)?;
    Ok(Writer {
        source: VerifiedMetadata {
            report,
            marker,
            connection,
            _lock,
        },
        database,
    })
}

/// Read committed WAL explicitly without guessing from only the main database file.
///
/// `root` is canonical and `lock` retains exclusive ownership. Returns a checked
/// `VerifiedMetadata` using a normal read-only view for live WAL or immutable mode
/// for checkpointed data. No rollback journal is repaired by this workflow.
fn recovery_reader(root: PathBuf, lock: File) -> Result<VerifiedMetadata> {
    match fs::symlink_metadata(root.join(MIGRATION_FILE)) {
        Ok(_) => {
            return Err(StorageError::NeedsRecovery(
                "finish metadata migration before job recovery",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io("inspect migration journal", &root, error)),
    }
    let marker = read_marker(&root)?;
    if marker.schema_version != 3 {
        return Err(StorageError::JobsUnavailable {
            found: marker.schema_version,
        });
    }
    let path = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
    let mut file = open_regular(&path)?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header)
        .map_err(|error| io("read job database header", &path, error))?;
    if &header[..16] != b"SQLite format 3\0" || header[18] != 2 || header[19] != 2 {
        return Err(StorageError::InvalidJob(
            "job recovery requires a WAL-format database",
        ));
    }
    let live_wal = checked_sidecars(&root, true)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = if live_wal {
        Connection::open_with_flags(&path, flags)?
    } else {
        Connection::open_with_flags(readonly_uri(&path), flags | OpenFlags::SQLITE_OPEN_URI)?
    };
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    verify_database(&connection, &marker)?;
    let report = MetadataReport {
        root,
        server_id: marker.server_id,
        sync_epoch: marker.sync_epoch,
        format_version: marker.format_version,
        schema_version: marker.schema_version,
        change_seq: read_change_seq(&connection, 3)?,
        journal_mode: "wal".to_owned(),
        integrity_check: "ok".to_owned(),
    };
    Ok(VerifiedMetadata {
        report,
        marker,
        connection,
        _lock: lock,
    })
}

/// Refuse unsafe SQLite sidecars and optionally recognize a committed WAL.
///
/// `root` selects metadata; `allow_wal` is true only for explicit recovery.
/// Returns `Result<bool>` indicating a nonempty WAL. Symlinks, special files,
/// nonempty rollback journals, and unintended live WAL are refused unchanged.
fn checked_sidecars(root: &Path, allow_wal: bool) -> Result<bool> {
    let mut live = false;
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = root
            .join(DATABASE_DIRECTORY)
            .join(format!("{DATABASE_FILE}{suffix}"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                open_regular(&path)?;
                if metadata.len() > 0 && suffix == "-journal" {
                    return Err(StorageError::NeedsRecovery(
                        "unexpected rollback journal prevents job writes",
                    ));
                }
                if metadata.len() > 0 && suffix == "-wal" {
                    if !allow_wal {
                        return Err(StorageError::NeedsRecovery(
                            "run explicit job recovery before writes",
                        ));
                    }
                    live = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("inspect job SQLite sidecar", &path, error)),
        }
    }
    Ok(live)
}

/// Flush a checked writer before releasing its maintenance lock or acknowledging work.
///
/// `writer` owns all resources. Returns `Result<()>` after integrity/history checks,
/// an explicit checkpoint, connection close, and database/directory synchronization.
/// A failure can leave committed WAL or an accepted outcome for explicit recovery.
fn finish_writer(writer: Writer) -> Result<()> {
    let Writer { source, database } = writer;
    verify_database(&source.connection, &source.marker)?;
    verify_all_jobs(&source.connection)?;
    let (busy, _, _): (i64, i64, i64) =
        source
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
    if busy != 0 {
        return Err(StorageError::NeedsRecovery("job checkpoint is busy"));
    }
    let VerifiedMetadata {
        report,
        connection,
        _lock,
        ..
    } = source;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    database
        .sync_all()
        .map_err(|error| io("synchronize job database", &report.root, error))?;
    sync_directory(&report.root.join(DATABASE_DIRECTORY))?;
    Ok(())
}

/// Close a diagnostic connection while retaining its lock until reading finishes.
///
/// `source` owns the checked reader. Returns `Result<()>` after successful close.
fn finish_reader(source: VerifiedMetadata) -> Result<()> {
    source
        .connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(())
}

/// Require the schema that actually contains the persistent job ledger.
///
/// `report` describes checked metadata. Returns `Result<()>` for schema 3, or an
/// actionable `JobsUnavailable` error without silently upgrading an older root.
fn require_jobs(report: &MetadataReport) -> Result<()> {
    if report.schema_version == 3 {
        Ok(())
    } else {
        Err(StorageError::JobsUnavailable {
            found: report.schema_version,
        })
    }
}

/// Resolve a job destination into a bounded, portable stored intent.
///
/// `root` is canonical metadata, `output` is the requested path, and `new` requires
/// absence during enqueue. Returns an absolute UTF-8 `PathBuf` outside metadata.
/// Existing paths are checked again by actual snapshot publication at execution.
fn checked_output(root: &Path, output: &Path, new: bool) -> Result<PathBuf> {
    let (parent, name) = destination(output)?;
    let output = parent.join(name);
    let text = output
        .to_str()
        .ok_or(StorageError::InvalidJob("job output path must be UTF-8"))?;
    if text.is_empty()
        || text.len() > 4096
        || text.as_bytes().contains(&0)
        || output.starts_with(root)
    {
        return Err(StorageError::InvalidJob(
            "job output must be a bounded path outside metadata",
        ));
    }
    if new {
        reject_existing(&output)?;
    }
    Ok(output)
}

/// Advance the shared lifecycle and persist its complete next projection together.
///
/// `source` owns the writer; `job` and `lifecycle` are the checked current pair.
/// `state`, `result`, and `error` describe the new outcome. Returns `Result<()>`
/// after one atomic job/outbox/watermark commit, or leaves durable state unchanged.
fn transition(
    source: &mut VerifiedMetadata,
    job: &mut SnapshotJob,
    lifecycle: &mut JobLifecycle,
    state: JobState,
    result: Option<SnapshotReport>,
    error: Option<String>,
) -> Result<()> {
    let before = job.clone();
    lifecycle.transition(lifecycle.revision(), state)?;
    job.state = lifecycle.state();
    job.revision = lifecycle.revision();
    job.attempt_generation = lifecycle.attempt_generation();
    job.updated_at_ms = now_ms()?;
    job.result = result;
    job.error = error;
    persist_change(source, Some(&before), job)
}

/// Commit one job projection, its change batch, and the next global position atomically.
///
/// `source` is the exclusively held writer; `before` is absent for creation or
/// supplies the expected old revision/state/token; `next` receives its initial
/// created sequence when new. Returns `Result<()>` only after a successful
/// `BEGIN IMMEDIATE` commit. No filesystem copy or waiting occurs in this transaction.
fn persist_change(
    source: &mut VerifiedMetadata,
    before: Option<&SnapshotJob>,
    next: &mut SnapshotJob,
) -> Result<()> {
    let transaction = source
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous: i64 = transaction.query_row(
        "SELECT change_seq FROM server_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let seq = previous
        .checked_add(1)
        .ok_or(StorageError::ChangeSequenceExhausted)?;
    let sequence = positive(seq)?;
    if before.is_none() {
        next.created_seq = sequence;
    }
    validate_projection(next)?;
    let payload = serde_json::to_string(next)
        .map_err(|_| StorageError::InvalidJob("job projection cannot be encoded"))?;
    if payload.len() > MAX_PROJECTION_BYTES {
        return Err(StorageError::InvalidJob("job projection exceeds 64 KiB"));
    }
    let result = next
        .result
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| StorageError::InvalidJob("snapshot result cannot be encoded"))?;
    let id = next.job_id.as_uuid().as_bytes().as_slice();
    let revision = number(next.revision.value())?;
    let generation = number(next.attempt_generation.value())?;
    let state = state_name(next.state)?;
    let output = next
        .output_path
        .to_str()
        .ok_or(StorageError::InvalidJob("job output is not UTF-8"))?;
    if let Some(before) = before {
        let changed = transaction.execute(
            "UPDATE jobs SET state=?1,revision=?2,attempt_generation=?3,updated_at_ms=?4,result_json=?5,error=?6 WHERE job_id=?7 AND revision=?8 AND state=?9 AND attempt_generation=?10",
            params![state, revision, generation, next.updated_at_ms, result, next.error, id,
                number(before.revision.value())?, state_name(before.state)?, number(before.attempt_generation.value())?],
        )?;
        if changed != 1 {
            return Err(StorageError::InvalidJob(
                "job changed before its update committed",
            ));
        }
    } else {
        transaction.execute("INSERT INTO jobs(job_id,output_path,state,revision,attempt_generation,created_seq,created_at_ms,updated_at_ms,result_json,error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![id, output, state, revision, generation, seq, next.created_at_ms, next.updated_at_ms, result, next.error])?;
    }
    transaction.execute("INSERT INTO change_batches(seq,job_id,revision,state,attempt_generation,payload_version,payload_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,1,?6,?7)",
        params![seq, id, revision, state, generation, payload, next.updated_at_ms])?;
    let changed = transaction.execute(
        "UPDATE server_state SET change_seq=?1 WHERE singleton=1 AND change_seq=?2",
        params![seq, previous],
    )?;
    if changed != 1 {
        return Err(StorageError::InvalidJob(
            "change position changed during job commit",
        ));
    }
    transaction.commit()?;
    source.report.change_seq = Some(
        Counter::from_u64(sequence.value()).map_err(|_| StorageError::ChangeSequenceExhausted)?,
    );
    Ok(())
}

/// Load a row and prove its bounded history reaches exactly the stored projection.
///
/// `connection` is checked metadata and `id` is the requested `JobId`. Returns
/// `Result<(SnapshotJob, JobLifecycle)>` after validating all payloads and replaying
/// transitions. Missing rows and any inconsistent history are explicit failures.
fn load_job(connection: &Connection, id: JobId) -> Result<(SnapshotJob, JobLifecycle)> {
    let raw = connection.query_row("SELECT output_path,state,revision,attempt_generation,created_seq,created_at_ms,updated_at_ms,result_json,error FROM jobs WHERE job_id=?1",
        [id.as_uuid().as_bytes().as_slice()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,i64>(6)?,row.get::<_,Option<String>>(7)?,row.get::<_,Option<String>>(8)?)))
        .optional()?.ok_or(StorageError::JobNotFound(id))?;
    let job = SnapshotJob {
        job_id: id,
        output_path: PathBuf::from(raw.0),
        state: parse_state(&raw.1)?,
        revision: positive(raw.2)?,
        attempt_generation: counter(raw.3)?,
        created_seq: positive(raw.4)?,
        created_at_ms: raw.5,
        updated_at_ms: raw.6,
        result: raw
            .7
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|_| StorageError::InvalidJob("invalid saved snapshot result"))?,
        error: raw.8,
    };
    validate_projection(&job)?;
    let mut statement = connection.prepare("SELECT seq,revision,state,attempt_generation,payload_version,payload_json,created_at_ms FROM change_batches WHERE job_id=?1 ORDER BY revision LIMIT 17")?;
    let mut rows = statement.query([id.as_uuid().as_bytes().as_slice()])?;
    let mut lifecycle = JobLifecycle::new();
    let mut count = 0;
    let mut previous_seq = 0;
    let mut captured_position = None;
    let (server_id, sync_epoch): (Vec<u8>, Vec<u8>) = connection.query_row(
        "SELECT server_id,sync_epoch FROM server_state WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut last = None;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > MAX_HISTORY_ROWS {
            return Err(StorageError::InvalidJob(
                "job history exceeds the closed runner's bound",
            ));
        }
        let seq = positive(row.get(0)?)?;
        let revision = positive(row.get(1)?)?;
        let state = parse_state(&row.get::<_, String>(2)?)?;
        let generation = counter(row.get(3)?)?;
        let version: i64 = row.get(4)?;
        let payload: String = row.get(5)?;
        let timestamp: i64 = row.get(6)?;
        if version != 1 || payload.len() > MAX_PROJECTION_BYTES || seq.value() <= previous_seq {
            return Err(StorageError::InvalidJob("invalid job change batch"));
        }
        let projection: SnapshotJob = serde_json::from_str(&payload)
            .map_err(|_| StorageError::InvalidJob("invalid job change projection"))?;
        validate_projection(&projection)?;
        if count == 1 {
            if revision != PositiveCounter::ONE
                || state != JobState::Queued
                || generation != Counter::ZERO
                || seq != job.created_seq
            {
                return Err(StorageError::InvalidJob(
                    "job history does not begin at accepted queued work",
                ));
            }
        } else {
            lifecycle
                .transition(lifecycle.revision(), state)
                .map_err(|_| StorageError::InvalidJob("illegal persisted job transition"))?;
        }
        if state == JobState::Running {
            captured_position = Some(
                Counter::from_u64(seq.value())
                    .map_err(|_| StorageError::InvalidJob("invalid captured snapshot position"))?,
            );
        }
        if let Some(result) = projection.result.as_ref()
            && (result.server_id.as_uuid().as_bytes().as_slice() != server_id.as_slice()
                || result.sync_epoch.as_uuid().as_bytes().as_slice() != sync_epoch.as_slice()
                || result.schema_version != 3
                || result.scope != SnapshotScope::Metadata
                || result.format_version != crate::FORMAT_VERSION
                || result.snapshot_format_version != crate::SNAPSHOT_FORMAT_VERSION
                || result.database_bytes == Counter::ZERO
                || result.change_seq != captured_position)
        {
            return Err(StorageError::InvalidJob(
                "snapshot result does not describe this job's captured source",
            ));
        }
        if lifecycle.state() != state
            || lifecycle.revision() != revision
            || lifecycle.attempt_generation() != generation
            || projection.job_id != id
            || projection.output_path != job.output_path
            || projection.created_seq != job.created_seq
            || projection.created_at_ms != job.created_at_ms
            || projection.state != state
            || projection.revision != revision
            || projection.attempt_generation != generation
            || projection.updated_at_ms != timestamp
        {
            return Err(StorageError::InvalidJob(
                "job projection disagrees with its history",
            ));
        }
        previous_seq = seq.value();
        last = Some(projection);
    }
    if last.as_ref() != Some(&job) {
        return Err(StorageError::InvalidJob(
            "job row disagrees with its last change projection",
        ));
    }
    Ok((job, lifecycle))
}

/// Replay each stored job before a writer is permitted to mutate the ledger.
///
/// `connection` is an already structurally verified database. Returns `Result<()>`
/// after streaming IDs and validating each bounded history, without collecting an
/// unbounded set of records in memory.
pub(super) fn verify_all_jobs(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("SELECT job_id FROM jobs")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        load_job(connection, parse_id(&row.get::<_, Vec<u8>>(0)?)?)?;
    }
    Ok(())
}

/// Collect selected IDs in stable acceptance order for bounded list or recovery.
///
/// `connection` is checked metadata; `limit` bounds diagnostics or is absent for
/// explicit recovery of active jobs. Queued backlogs are not collected by recovery.
/// Returns `Result<Vec<JobId>>`.
fn job_ids(connection: &Connection, limit: Option<u16>) -> Result<Vec<JobId>> {
    let sql = if limit.is_some() {
        "SELECT job_id FROM jobs ORDER BY created_seq DESC LIMIT ?1"
    } else {
        "SELECT job_id FROM jobs WHERE state IN ('running','verifying') ORDER BY created_seq"
    };
    let mut statement = connection.prepare(sql)?;
    let mut rows = if let Some(limit) = limit {
        statement.query([i64::from(limit)])?
    } else {
        statement.query([])?
    };
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(parse_id(&row.get::<_, Vec<u8>>(0)?)?);
    }
    Ok(ids)
}

/// Check the shape of one current or historical public job projection.
///
/// `job` is decoded data, not authority. Returns `Result<()>` for the closed
/// snapshot workflow's bounded fields and matching outcome type.
fn validate_projection(job: &SnapshotJob) -> Result<()> {
    let output = job
        .output_path
        .to_str()
        .ok_or(StorageError::InvalidJob("job output is not UTF-8"))?;
    state_name(job.state)?;
    if !job.output_path.is_absolute()
        || output.is_empty()
        || output.len() > 4096
        || output.as_bytes().contains(&0)
        || job.created_at_ms < 0
        || job.updated_at_ms < 0
        || job.attempt_generation.value() >= job.revision.value()
        || (matches!(job.state, JobState::Running | JobState::Verifying)
            && job.attempt_generation == Counter::ZERO)
        || (job.state == JobState::Completed) != job.result.is_some()
        || (job.state == JobState::Failed) != job.error.is_some()
        || job
            .error
            .as_ref()
            .is_some_and(|error| error.is_empty() || error.len() > MAX_ERROR_BYTES)
        || job
            .result
            .as_ref()
            .is_some_and(|result| result.root != job.output_path)
    {
        return Err(StorageError::InvalidJob(
            "job projection violates the snapshot contract",
        ));
    }
    Ok(())
}

/// Parse the UUID bytes stored in a job row through the domain identity rules.
///
/// `bytes` is an untrusted stored slice. Returns a validated `JobId` or InvalidJob.
fn parse_id(bytes: &[u8]) -> Result<JobId> {
    let uuid =
        Uuid::from_slice(bytes).map_err(|_| StorageError::InvalidJob("invalid stored job UUID"))?;
    JobId::from_uuid(uuid).map_err(|_| StorageError::InvalidJob("stored job ID must be UUIDv7"))
}

/// Return the fixed SQL/wire spelling of a state supported by this runner.
///
/// `state` is a domain `JobState`. Returns `Result<&'static str>`; unrelated
/// scheduler states remain unavailable rather than pretending execution exists.
fn state_name(state: JobState) -> Result<&'static str> {
    match state {
        JobState::Queued => Ok("queued"),
        JobState::Running => Ok("running"),
        JobState::Verifying => Ok("verifying"),
        JobState::Completed => Ok("completed"),
        JobState::Failed => Ok("failed"),
        JobState::Canceled => Ok("canceled"),
        _ => Err(StorageError::InvalidJob(
            "state is unavailable to the local snapshot runner",
        )),
    }
}

/// Decode a stored state using exactly the local runner's closed vocabulary.
///
/// `state` is stored text. Returns `Result<JobState>` or a ledger-validation error.
fn parse_state(state: &str) -> Result<JobState> {
    match state {
        "queued" => Ok(JobState::Queued),
        "running" => Ok(JobState::Running),
        "verifying" => Ok(JobState::Verifying),
        "completed" => Ok(JobState::Completed),
        "failed" => Ok(JobState::Failed),
        "canceled" => Ok(JobState::Canceled),
        _ => Err(StorageError::InvalidJob(
            "unsupported stored snapshot state",
        )),
    }
}

/// Check a stored nonnegative integer before representing it as a domain counter.
///
/// `value` is an SQLite `i64`. Returns `Result<Counter>` without coercing negatives.
fn counter(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::InvalidJob(
            "invalid stored nonnegative counter",
        ))
}

/// Check a stored positive integer used for revisions and change positions.
///
/// `value` is an SQLite `i64`. Returns `Result<PositiveCounter>`; zero is refused.
fn positive(value: i64) -> Result<PositiveCounter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| PositiveCounter::from_u64(value).ok())
        .ok_or(StorageError::InvalidJob("invalid stored positive counter"))
}

/// Convert a validated counter to SQLite's signed integer representation.
///
/// `value` is an unsigned counter value. Returns `Result<i64>` without truncation.
fn number(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StorageError::InvalidJob("counter exceeds SQLite range"))
}

/// Observe server UTC milliseconds for display, never for transition ordering.
///
/// Returns `Result<i64>`; clocks before the epoch or outside SQLite's supported
/// range fail before a transaction. Revisions and change positions establish order.
fn now_ms() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::InvalidJob("server clock precedes the Unix epoch"))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| StorageError::InvalidJob("server timestamp exceeds SQLite range"))
}

/// Keep a failure actionable without storing an unbounded diagnostic string.
///
/// `error` is the snapshot failure. Returns nonempty UTF-8 `String` of at most
/// 4096 bytes, retaining artifacts rather than attempting automatic deletion.
fn bounded_error(error: &StorageError) -> String {
    let mut message = error.to_string();
    if message.len() > MAX_ERROR_BYTES {
        let mut boundary = MAX_ERROR_BYTES;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
    }
    if message.is_empty() {
        "snapshot operation failed".to_owned()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exit without destructors at a real run boundary selected only by unit tests.
    #[test]
    fn crash_child() {
        let Some(root) = std::env::var_os("JUMPSTART_JOB_CRASH_ROOT") else {
            return;
        };
        let job_id: JobId = std::env::var("JUMPSTART_JOB_CRASH_ID")
            .unwrap()
            .parse()
            .unwrap();
        let phase = std::env::var("JUMPSTART_JOB_CRASH_PHASE").unwrap();
        run_with_hook(Path::new(&root), job_id, |step| {
            if format!("{step:?}") == phase {
                std::process::exit(94);
            }
            Ok(())
        })
        .unwrap();
        panic!("requested job crash boundary was not reached");
    }

    /// Recover actual committed WAL after process exit without accepting output alone.
    #[test]
    fn interrupted_runs_are_fenced_and_completed_commits_survive() {
        let _isolation = crate::isolate_process_tests();
        for phase in [
            RunStep::RunningCommitted,
            RunStep::SnapshotPublished,
            RunStep::CompletedCommitted,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("metadata");
            let output = temporary.path().join("snapshot");
            crate::initialize(&root).unwrap();
            let job = enqueue_snapshot_job(&root, &output).unwrap();
            let queued =
                enqueue_snapshot_job(&root, &temporary.path().join("later-snapshot")).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::jobs::tests::crash_child",
                    "--nocapture",
                ])
                .env("JUMPSTART_JOB_CRASH_ROOT", &root)
                .env("JUMPSTART_JOB_CRASH_ID", job.job_id.to_string())
                .env("JUMPSTART_JOB_CRASH_PHASE", format!("{phase:?}"))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(94));
            assert!(matches!(
                crate::inspect(&root),
                Err(StorageError::NeedsRecovery(_))
            ));
            let recovered = recover_jobs(&root).unwrap();
            let stored = inspect_job(&root, job.job_id).unwrap();
            assert_eq!(inspect_job(&root, queued.job_id).unwrap(), queued);
            if phase == RunStep::CompletedCommitted {
                assert!(recovered.is_empty());
                assert_eq!(stored.state, JobState::Completed);
                assert!(stored.result.is_some());
            } else {
                assert_eq!(recovered.len(), 1);
                assert_eq!(stored.state, JobState::Failed);
                assert!(stored.attempt_generation.value() > 1);
                assert!(stored.result.is_none());
            }
            if phase != RunStep::RunningCommitted {
                assert!(snapshot::verify_snapshot(&output).is_ok());
            }
        }
    }
}
