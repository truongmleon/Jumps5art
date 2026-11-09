//! Extends the volume journal to record backup and primary intents together.
//!
//! Both roles share one root and filesystem uniqueness boundary. An existing
//! primary keeps its exact IDs and marker when the journal is upgraded.

use rusqlite::{Connection, TransactionBehavior, params};

use super::schema5;
use crate::{Result, StorageError};

pub(super) const MIGRATION_NAME: &str = "backup_volume_registration";

const RENAME_V5: &str = "ALTER TABLE volume_registrations RENAME TO volume_registrations_v5";
const COPY_V5: &str = "INSERT INTO volume_registrations
    (plan_id,root_path,filesystem_uuid,directory_inode,directory_birth,server_id,
     volume_id,primary_id,registration_nonce,registered_volume_id,role)
    SELECT plan_id,root_path,filesystem_uuid,directory_inode,directory_birth,server_id,
           volume_id,primary_id,registration_nonce,registered_volume_id,'primary'
    FROM volume_registrations_v5";
const DROP_V5: &str = "DROP TABLE volume_registrations_v5";

const REGISTRATIONS: &str = "CREATE TABLE volume_registrations (
    plan_id BLOB PRIMARY KEY CHECK (length(plan_id) = 16),
    root_path TEXT NOT NULL UNIQUE,
    filesystem_uuid BLOB NOT NULL UNIQUE CHECK (length(filesystem_uuid) = 16),
    directory_inode TEXT NOT NULL,
    directory_birth TEXT NOT NULL,
    server_id BLOB NOT NULL CHECK (length(server_id) = 16),
    volume_id BLOB NOT NULL UNIQUE CHECK (length(volume_id) = 16),
    primary_id BLOB UNIQUE CHECK (primary_id IS NULL OR length(primary_id) = 16),
    registration_nonce BLOB NOT NULL UNIQUE CHECK (length(registration_nonce) = 16),
    registered_volume_id BLOB UNIQUE REFERENCES volumes(volume_id),
    role TEXT NOT NULL CHECK (role IN ('primary','backup')),
    CHECK ((role = 'primary') = (primary_id IS NOT NULL)),
    CHECK (registered_volume_id IS NULL OR registered_volume_id = volume_id)
) STRICT";

/// Return the schema objects expected after replacing the registration journal.
pub(super) fn statements() -> [&'static str; 2] {
    [schema5::MAINTENANCE, REGISTRATIONS]
}

/// Fingerprint the entire journal conversion, including how old rows are copied.
pub(super) fn checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    for statement in [RENAME_V5, REGISTRATIONS, COPY_V5, DROP_V5] {
        hasher.update(statement.as_bytes());
        hasher.update(b";\n");
    }
    format!("b3:{}", hasher.finalize())
}

/// Replace the primary-only journal transactionally while preserving all rows.
pub(super) fn apply(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 5 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 5,
        });
    }
    for statement in [RENAME_V5, REGISTRATIONS, COPY_V5, DROP_V5] {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO schema_migrations (version,name,checksum,applied_at)
         VALUES (6,?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        params![MIGRATION_NAME, checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 6)?;
    transaction.commit()?;
    Ok(())
}

/// Check that each accepted intent belongs to this server and matches its catalog row.
pub(super) fn verify_state(connection: &Connection) -> Result<()> {
    let valid: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM local_admin_state WHERE singleton=1)
         AND NOT EXISTS (SELECT 1 FROM volume_registrations AS r
             WHERE r.server_id != (SELECT server_id FROM server_state WHERE singleton=1))
         AND NOT EXISTS (SELECT 1 FROM volume_registrations AS r
             JOIN volumes AS v ON v.volume_id=r.registered_volume_id
             WHERE v.role!=r.role OR v.primary_id IS NOT r.primary_id
                OR lower(replace(v.filesystem_identity,'-',''))!=lower(hex(r.filesystem_uuid)))",
        [],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(StorageError::Integrity("invalid volume registration state"));
    }
    Ok(())
}
