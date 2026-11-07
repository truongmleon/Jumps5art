//! Defines the durable snapshot-job records and their transactionally paired changes.
//!
//! A job's current status is useful for display, while its ordered changes explain
//! how it reached that status after a restart. Deferred foreign keys let one
//! transaction insert/update both sides together, then reject a partial result at
//! commit. The global change floor preserves an older schema's captured position.

use rusqlite::Connection;

use crate::{Result, StorageError};

pub(super) const JOBS_TABLE: &str = r#"CREATE TABLE jobs (
    job_id BLOB PRIMARY KEY CHECK (length(job_id) = 16),
    output_path TEXT NOT NULL CHECK (length(CAST(output_path AS BLOB)) BETWEEN 1 AND 4096 AND instr(output_path, char(0)) = 0),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'verifying', 'completed', 'failed', 'canceled')),
    revision INTEGER NOT NULL CHECK (revision > 0),
    attempt_generation INTEGER NOT NULL CHECK (attempt_generation >= 0 AND attempt_generation < revision),
    created_seq INTEGER NOT NULL UNIQUE CHECK (created_seq > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    result_json TEXT CHECK (result_json IS NULL OR (json_valid(result_json) AND length(CAST(result_json AS BLOB)) <= 65536)),
    error TEXT CHECK (error IS NULL OR length(CAST(error AS BLOB)) BETWEEN 1 AND 4096),
    CHECK ((state = 'completed') = (result_json IS NOT NULL)),
    CHECK ((state = 'failed') = (error IS NOT NULL)),
    CHECK (state NOT IN ('running', 'verifying') OR attempt_generation > 0),
    FOREIGN KEY (job_id, revision, state, attempt_generation) REFERENCES change_batches(job_id, revision, state, attempt_generation) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (created_seq) REFERENCES change_batches(seq) DEFERRABLE INITIALLY DEFERRED
) STRICT"#;

pub(super) const CHANGE_BATCHES_TABLE: &str = r#"CREATE TABLE change_batches (
    seq INTEGER PRIMARY KEY CHECK (seq > 0),
    job_id BLOB NOT NULL CHECK (length(job_id) = 16),
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'verifying', 'completed', 'failed', 'canceled')),
    attempt_generation INTEGER NOT NULL CHECK (attempt_generation >= 0 AND attempt_generation < revision),
    payload_version INTEGER NOT NULL CHECK (payload_version = 1),
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json) AND length(CAST(payload_json AS BLOB)) <= 65536),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    UNIQUE (job_id, revision),
    UNIQUE (job_id, revision, state, attempt_generation),
    FOREIGN KEY (job_id) REFERENCES jobs(job_id)
) STRICT"#;

pub(super) const JOBS_QUEUE_INDEX: &str = "CREATE INDEX jobs_queue ON jobs(state, created_seq)";

pub(super) const FLOOR_MIGRATION: &str = "ALTER TABLE server_state ADD COLUMN change_floor INTEGER NOT NULL DEFAULT 0 CHECK (change_floor >= 0 AND change_floor <= change_seq)";
pub(super) const INITIALIZE_FLOOR: &str =
    "UPDATE server_state SET change_floor = change_seq WHERE singleton = 1";
pub(super) const MIGRATION_NAME: &str = "snapshot_job_ledger";

/// Return the exact ordered statements used for the schema-3 migration checksum.
///
/// # Returns
/// Five fixed SQL statements: floor column, floor initialization, both tables,
/// and the queue index. Each is executed inside the same write transaction.
pub(super) fn statements() -> [&'static str; 5] {
    [
        FLOOR_MIGRATION,
        INITIALIZE_FLOOR,
        JOBS_TABLE,
        CHANGE_BATCHES_TABLE,
        JOBS_QUEUE_INDEX,
    ]
}

/// Fingerprint all schema-3 statements so a changed migration cannot be adopted.
///
/// # Returns
/// The tagged BLAKE3 checksum of exact statements with their fixed separators.
pub(super) fn checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    for statement in statements() {
        hasher.update(statement.as_bytes());
        hasher.update(b";\n");
    }
    format!("b3:{}", hasher.finalize().to_hex())
}

/// Check that retained changes exactly cover the stored floor-to-high-water range.
///
/// # Arguments
/// * `connection` — a schema-3 `&Connection` protected by its metadata lock.
///
/// # Returns
/// `Result<()>` when floor/count/endpoints agree. Empty history is valid only at
/// its preserved floor. Job payload decoding and lifecycle replay are separate.
///
/// # Errors
/// Rejects negative/inverted positions, missing changes, inconsistent endpoints,
/// or SQLite failures. It does not modify or reconstruct ledger rows.
pub(super) fn verify_ledger(connection: &Connection) -> Result<()> {
    let (floor, high_water): (i64, i64) = connection.query_row(
        "SELECT change_floor, change_seq FROM server_state WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if floor < 0 || high_water < floor {
        return Err(StorageError::Integrity("invalid change-history floor"));
    }
    let (count, first, last): (i64, Option<i64>, Option<i64>) = connection.query_row(
        "SELECT count(*), min(seq), max(seq) FROM change_batches",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if count != high_water - floor {
        return Err(StorageError::Integrity(
            "change-history range has missing rows",
        ));
    }
    if count == 0 {
        if first.is_some() || last.is_some() {
            return Err(StorageError::Integrity(
                "empty change history has endpoints",
            ));
        }
    } else if first != floor.checked_add(1) || last != Some(high_water) {
        return Err(StorageError::Integrity("change-history endpoints disagree"));
    }
    Ok(())
}
