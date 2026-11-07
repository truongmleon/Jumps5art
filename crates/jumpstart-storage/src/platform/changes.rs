//! Reads the saved changes across local snapshot jobs using a resumable cursor.
//!
//! For example, a local observer that last saw position 12 can ask for complete
//! later projections without confusing them with today's current job rows. The
//! epoch binds that cursor to one synchronization history. The shared metadata
//! lock keeps page bounds and records in the same checked offline view.
//! This is a local job-feed preview, not an authenticated or general sync service.

use std::path::Path;

use jumpstart_domain::{Counter, PositiveCounter, SyncEpoch};
use rusqlite::params;
use serde::Serialize;

use super::open_verified_metadata;
use crate::{
    MAX_JOB_CHANGE_PAGE_BYTES, MAX_JOB_CHANGE_PAGE_RECORDS, Result, SnapshotJob, SnapshotJobChange,
    SnapshotJobChangePage, StorageError,
};

const MAX_PROJECTION_BYTES: usize = 65_536;

/// Read whole job-change projections strictly after the caller's saved position.
///
/// # Arguments
///
/// * `root` — existing metadata with a job ledger `&Path`, opened read-only without repair.
/// * `epoch` — `SyncEpoch` of the history from which the caller obtained its cursor.
/// * `after` — nonnegative `Counter` position already observed by the caller.
/// * `limit` — requested record cap in `1..=MAX_JOB_CHANGE_PAGE_RECORDS`.
///
/// # Returns
///
/// `Result<SnapshotJobChangePage>` with original full projections in ascending
/// position order. `next` is the last included position, or `after` for an empty
/// page. The entire compact serialized page stays within the 1 MiB bound; a batch
/// is never split to fit. The captured high-water mark can exceed `next` when more
/// changes remain for a later pull.
///
/// # Errors
///
/// Invalid limits fail before filesystem access. Unsupported schemas, a wrong
/// epoch, expired or future cursors, invalid stored data, active writers, pending
/// recovery, and filesystem/SQLite failures are explicit errors. No cursor is
/// silently clamped and no job or acknowledgment is changed by this read.
pub(crate) fn pull_job_changes(
    root: &Path,
    epoch: SyncEpoch,
    after: Counter,
    limit: u16,
) -> Result<SnapshotJobChangePage> {
    if !(1..=MAX_JOB_CHANGE_PAGE_RECORDS).contains(&limit) {
        return Err(StorageError::InvalidJobChangeCursor(
            "job change limit is outside the supported page range",
        ));
    }
    let source = open_verified_metadata(root)?;
    let outcome = (|| -> Result<SnapshotJobChangePage> {
        if source.report.schema_version < 3 {
            return Err(StorageError::JobsUnavailable {
                found: source.report.schema_version,
            });
        }
        if epoch != source.report.sync_epoch {
            return Err(StorageError::JobChangeEpochMismatch {
                requested: epoch,
                current: source.report.sync_epoch,
            });
        }
        let (floor, high): (i64, i64) = source.connection.query_row(
            "SELECT change_floor,change_seq FROM server_state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let minimum = stored_counter(floor)?;
        let high_water = stored_counter(high)?;
        if after < minimum {
            return Err(StorageError::JobChangesUnavailable { after, minimum });
        }
        if after > high_water {
            return Err(StorageError::InvalidJobChangeCursor(
                "job change cursor is ahead of the captured high-water mark",
            ));
        }
        let mut page = SnapshotJobChangePage {
            server_id: source.report.server_id,
            sync_epoch: source.report.sync_epoch,
            after,
            next: after,
            high_water,
            minimum,
            batches: Vec::new(),
        };
        let mut bytes = encoded_len(&page)?;
        if bytes > MAX_JOB_CHANGE_PAGE_BYTES {
            return Err(StorageError::InvalidJob(
                "job change page envelope exceeds its budget",
            ));
        }
        let cursor = i64::try_from(after.value()).map_err(|_| {
            StorageError::InvalidJobChangeCursor("job change cursor exceeds SQLite's range")
        })?;
        let mut statement = source.connection.prepare(
            "SELECT seq,payload_version,payload_json FROM change_batches WHERE seq>?1 AND seq<=?2 ORDER BY seq LIMIT ?3",
        )?;
        let mut rows = statement.query(params![cursor, high, i64::from(limit)])?;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let payload_version: i64 = row.get(1)?;
            let payload: String = row.get(2)?;
            if payload_version != 1 || payload.len() > MAX_PROJECTION_BYTES {
                return Err(StorageError::InvalidJob("invalid job change payload"));
            }
            let seq = u64::try_from(sequence)
                .ok()
                .and_then(|value| PositiveCounter::from_u64(value).ok())
                .ok_or(StorageError::InvalidJob("invalid job change sequence"))?;
            let job: SnapshotJob = serde_json::from_str(&payload)
                .map_err(|_| StorageError::InvalidJob("invalid job change projection"))?;
            let batch = SnapshotJobChange {
                seq,
                payload_version: 1,
                job,
            };
            if !push_bounded(&mut page, &mut bytes, batch)? {
                break;
            }
        }
        Ok(page)
    })();
    // Queries and their borrowed rows are dropped by the closure before closing
    // the connection. The remaining owned lock field is released only afterward.
    let closed = source
        .connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error));
    let page = outcome?;
    closed?;
    Ok(page)
}

/// Append a complete batch only when its compact JSON fits both page caps.
///
/// `page` is the current result, `bytes` is its exact serialized size, and `batch`
/// is the next checked projection. Returns `Ok(true)` when appended; `Ok(false)`
/// leaves the page, its cursor, and byte count unchanged. Invalid ordering, a first
/// batch larger than the page, or failed encoding returns an error. Each batch is
/// encoded once for size accounting; commas and the encoded next cursor's growth
/// are counted separately.
fn push_bounded(
    page: &mut SnapshotJobChangePage,
    bytes: &mut usize,
    batch: SnapshotJobChange,
) -> Result<bool> {
    if batch.seq.value() <= page.next.value() {
        return Err(StorageError::InvalidJob(
            "job changes are not strictly ordered",
        ));
    }
    if page.batches.len() >= usize::from(MAX_JOB_CHANGE_PAGE_RECORDS) {
        return Ok(false);
    }
    let next = Counter::from_u64(batch.seq.value())
        .map_err(|_| StorageError::InvalidJob("invalid next job change position"))?;
    let cursor_growth = encoded_len(&next)?
        .checked_sub(encoded_len(&page.next)?)
        .ok_or(StorageError::InvalidJob(
            "encoded job cursor unexpectedly shrank",
        ))?;
    let separator = usize::from(!page.batches.is_empty());
    let candidate_bytes = bytes
        .checked_add(encoded_len(&batch)?)
        .and_then(|value| value.checked_add(separator))
        .and_then(|value| value.checked_add(cursor_growth))
        .ok_or(StorageError::InvalidJob("job change page size overflow"))?;
    if candidate_bytes > MAX_JOB_CHANGE_PAGE_BYTES {
        if page.batches.is_empty() {
            return Err(StorageError::InvalidJob(
                "one job change cannot fit within the page budget",
            ));
        }
        return Ok(false);
    }
    page.next = next;
    page.batches.push(batch);
    *bytes = candidate_bytes;
    Ok(true)
}

/// Measure the actual compact JSON representation used by the page contract.
///
/// `value` is serializable page data. Returns `Result<usize>` including escaped
/// text and punctuation; encoding failures are rejected rather than underestimated.
fn encoded_len(value: &impl Serialize) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| StorageError::InvalidJob("job change page cannot be encoded"))
}

/// Preserve a stored nonnegative SQLite position without truncation or coercion.
///
/// `value` is an `i64` bound read from schema 3. Returns `Result<Counter>`; a
/// negative value or unsupported range is a ledger error, not a fresh cursor.
fn stored_counter(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::InvalidJob("invalid stored job change bound"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use jumpstart_domain::{JobId, JobState, ServerId};

    use super::*;

    /// Make an empty synthetic page with a valid small cursor for accounting tests.
    ///
    /// `after` is the starting numeric position. Returns a page with fixed bounds;
    /// invalid fixture counters panic instead of weakening the production checks.
    fn empty_page(after: u64) -> SnapshotJobChangePage {
        let after = Counter::from_u64(after).unwrap();
        SnapshotJobChangePage {
            server_id: ServerId::new(),
            sync_epoch: SyncEpoch::new(),
            after,
            next: after,
            high_water: Counter::from_u64(after.value().max(100_000)).unwrap(),
            minimum: Counter::ZERO,
            batches: Vec::new(),
        }
    }

    /// Construct a valid queued projection without creating hundreds of real jobs.
    ///
    /// `sequence` is its acceptance position; `long` selects a bounded absolute
    /// path containing characters JSON must escape. Returns a complete change
    /// whose job and batch agree about the original acceptance position.
    fn change(sequence: u64, long: bool) -> SnapshotJobChange {
        let seq = PositiveCounter::from_u64(sequence).unwrap();
        let output_path = if long {
            let segment = "\u{1}".repeat(200);
            PathBuf::from(format!("/{segment}/{segment}/{segment}/{segment}/snapshot"))
        } else {
            PathBuf::from("/scratch/snapshot")
        };
        SnapshotJobChange {
            seq,
            payload_version: 1,
            job: SnapshotJob {
                job_id: JobId::new(),
                output_path,
                state: JobState::Queued,
                revision: PositiveCounter::ONE,
                attempt_generation: Counter::ZERO,
                created_seq: seq,
                created_at_ms: 1,
                updated_at_ms: 1,
                result: None,
                error: None,
            },
        }
    }

    /// Count commas, escaping, and decimal-string cursor growth exactly.
    #[test]
    fn page_accounting_matches_actual_compact_json() {
        let mut page = empty_page(9);
        let mut bytes = encoded_len(&page).unwrap();
        assert!(push_bounded(&mut page, &mut bytes, change(10, false)).unwrap());
        assert!(push_bounded(&mut page, &mut bytes, change(100, true)).unwrap());
        assert_eq!(page.after.value(), 9);
        assert_eq!(page.next.value(), 100);
        assert_eq!(bytes, serde_json::to_vec(&page).unwrap().len());
        assert!(bytes <= MAX_JOB_CHANGE_PAGE_BYTES);
    }

    /// Stop at the byte boundary without appending part of a batch or advancing next.
    #[test]
    fn byte_limit_preserves_the_last_complete_cursor() {
        let mut page = empty_page(9);
        let mut bytes = encoded_len(&page).unwrap();
        let mut sequence = 10;
        loop {
            let before_count = page.batches.len();
            let before_next = page.next;
            let before_bytes = bytes;
            let batch = change(sequence, true);
            if !push_bounded(&mut page, &mut bytes, batch.clone()).unwrap() {
                assert_eq!(page.batches.len(), before_count);
                assert_eq!(page.next, before_next);
                assert_eq!(bytes, before_bytes);
                assert!(page.batches.len() < usize::from(MAX_JOB_CHANGE_PAGE_RECORDS));
                assert!(bytes <= MAX_JOB_CHANGE_PAGE_BYTES);
                assert_eq!(bytes, serde_json::to_vec(&page).unwrap().len());
                let mut oversized = page.clone();
                oversized.next = Counter::from_u64(sequence).unwrap();
                oversized.batches.push(batch);
                assert!(serde_json::to_vec(&oversized).unwrap().len() > MAX_JOB_CHANGE_PAGE_BYTES);
                break;
            }
            sequence += 1;
        }
        // Current checked payloads cannot reach this size, but future growth
        // must fail explicitly rather than return an empty, non-progressing page.
        let mut empty = empty_page(0);
        let before = empty.clone();
        let mut bytes = encoded_len(&empty).unwrap();
        let before_bytes = bytes;
        let mut oversized = change(1, false);
        oversized.job.output_path =
            PathBuf::from(format!("/{}", "x".repeat(MAX_JOB_CHANGE_PAGE_BYTES)));
        assert!(push_bounded(&mut empty, &mut bytes, oversized).is_err());
        assert_eq!(empty, before);
        assert_eq!(bytes, before_bytes);
    }

    /// Apply the record cap independently of the byte cap, retaining the saved page.
    #[test]
    fn record_limit_does_not_split_or_consume_the_next_change() {
        let mut page = empty_page(0);
        let mut bytes = encoded_len(&page).unwrap();
        for sequence in 1..=u64::from(MAX_JOB_CHANGE_PAGE_RECORDS) {
            assert!(push_bounded(&mut page, &mut bytes, change(sequence, false)).unwrap());
        }
        assert!(bytes < MAX_JOB_CHANGE_PAGE_BYTES);
        let before = page.clone();
        let before_bytes = bytes;
        assert!(
            !push_bounded(
                &mut page,
                &mut bytes,
                change(u64::from(MAX_JOB_CHANGE_PAGE_RECORDS) + 1, false),
            )
            .unwrap()
        );
        assert_eq!(page, before);
        assert_eq!(bytes, before_bytes);
        assert_eq!(bytes, serde_json::to_vec(&page).unwrap().len());
    }
}
