//! Shows the recorded stages of one snapshot request without running it again.
//!
//! For example, a completed job's current row does not show when it entered
//! Running or Verifying. The saved full projections retain those earlier values.
//! This reader returns them in revision order under the same shared maintenance
//! lock used by ordinary inspection. It is not a global change-feed endpoint.

use std::path::Path;

use jumpstart_domain::JobId;

use super::open_verified_metadata;
use crate::{Result, SnapshotJob, StorageError};

const MAX_HISTORY_ROWS: usize = 16;
const MAX_PROJECTION_BYTES: usize = 65_536;

/// Read the accepted projections of one job from its first to latest revision.
///
/// # Arguments
///
/// * `root` — existing schema-3 metadata `&Path`, inspected without repair.
/// * `job_id` — validated `JobId` identifying the request whose history is wanted.
///
/// # Returns
///
/// `Result<Vec<SnapshotJob>>` containing the saved full values in revision order.
/// Earlier entries keep their original state and outcome fields; this operation
/// neither starts work nor updates a job. Decoding is capped at 16 records,
/// matching the current closed runner's history bound.
///
/// # Errors
///
/// Refuses an unsupported schema, unknown job, active writer, pending recovery,
/// invalid stored projection, or filesystem/SQLite failure. Opening the checked
/// source already validates its complete ledger and lifecycle histories.
pub(crate) fn job_history(root: &Path, job_id: JobId) -> Result<Vec<SnapshotJob>> {
    let source = open_verified_metadata(root)?;
    if source.report.schema_version != 3 {
        return Err(StorageError::JobsUnavailable {
            found: source.report.schema_version,
        });
    }
    let history = (|| -> Result<Vec<SnapshotJob>> {
        let mut statement = source.connection.prepare(
            "SELECT payload_version,payload_json FROM change_batches WHERE job_id=?1 ORDER BY revision LIMIT 17",
        )?;
        let mut rows = statement.query([job_id.as_uuid().as_bytes().as_slice()])?;
        let mut history = Vec::new();
        while let Some(row) = rows.next()? {
            let version: i64 = row.get(0)?;
            let payload: String = row.get(1)?;
            if history.len() == MAX_HISTORY_ROWS
                || version != 1
                || payload.len() > MAX_PROJECTION_BYTES
            {
                return Err(StorageError::InvalidJob("invalid or oversized job history"));
            }
            let projection: SnapshotJob = serde_json::from_str(&payload)
                .map_err(|_| StorageError::InvalidJob("invalid job history projection"))?;
            if projection.job_id != job_id {
                return Err(StorageError::InvalidJob(
                    "job history contains a different request identity",
                ));
            }
            history.push(projection);
        }
        Ok(history)
    })();
    // Close the reader before its still-owned maintenance-lock field is dropped,
    // including when decoding failed. A decoding failure remains the primary error.
    let closed = source
        .connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error));
    let history = history?;
    closed?;
    if history.is_empty() {
        return Err(StorageError::JobNotFound(job_id));
    }
    Ok(history)
}
