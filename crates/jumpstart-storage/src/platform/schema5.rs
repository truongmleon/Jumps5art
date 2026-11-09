//! Adds the persistent local administration state and volume registration journal.
//!
//! A pending row records the exact marker to publish. After the marker and its
//! directory are synchronized, a second transaction links the registered volume.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Result, StorageError};

pub(super) const MIGRATION_NAME: &str = "local_admin_registration";

pub(super) const MAINTENANCE: &str = "CREATE TABLE local_admin_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    maintenance INTEGER NOT NULL CHECK (maintenance IN (0, 1))
) STRICT";

const REGISTRATIONS: &str = "CREATE TABLE volume_registrations (
    plan_id BLOB PRIMARY KEY CHECK (length(plan_id) = 16),
    root_path TEXT NOT NULL UNIQUE,
    filesystem_uuid BLOB NOT NULL UNIQUE CHECK (length(filesystem_uuid) = 16),
    directory_inode TEXT NOT NULL,
    directory_birth TEXT NOT NULL,
    server_id BLOB NOT NULL CHECK (length(server_id) = 16),
    volume_id BLOB NOT NULL UNIQUE CHECK (length(volume_id) = 16),
    primary_id BLOB NOT NULL UNIQUE CHECK (length(primary_id) = 16),
    registration_nonce BLOB NOT NULL UNIQUE CHECK (length(registration_nonce) = 16),
    registered_volume_id BLOB UNIQUE REFERENCES volumes(volume_id),
    CHECK (registered_volume_id IS NULL OR registered_volume_id = volume_id)
) STRICT";

const SEED: &str = "INSERT INTO local_admin_state VALUES (1, 0)";

/// Return the exact schema objects expected from the registration migration.
pub(super) fn statements() -> [&'static str; 2] {
    [MAINTENANCE, REGISTRATIONS]
}

/// Fingerprint the definitions and initial state together.
pub(super) fn checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    for statement in [MAINTENANCE, REGISTRATIONS, SEED] {
        hasher.update(statement.as_bytes());
        hasher.update(b";\n");
    }
    format!("b3:{}", hasher.finalize())
}

/// Install the admin state and journal in one transaction on checked schema 4.
pub(super) fn apply(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 4 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 4,
        });
    }
    for statement in [MAINTENANCE, REGISTRATIONS, SEED] {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO schema_migrations (version,name,checksum,applied_at)
         VALUES (5,?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        params![MIGRATION_NAME, checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 5)?;
    transaction.commit()?;
    Ok(())
}

/// Refuse an incomplete singleton or a journal bound to another authority.
pub(super) fn verify_state(connection: &Connection) -> Result<()> {
    let valid: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM local_admin_state WHERE singleton=1)
         AND NOT EXISTS (SELECT 1 FROM volume_registrations AS r
             WHERE r.server_id != (SELECT server_id FROM server_state WHERE singleton=1))
         AND NOT EXISTS (SELECT 1 FROM volume_registrations AS r
             JOIN volumes AS v ON v.volume_id=r.registered_volume_id
             WHERE v.role!='primary' OR v.primary_id!=r.primary_id
                OR lower(replace(v.filesystem_identity,'-',''))!=lower(hex(r.filesystem_uuid)))",
        [],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(StorageError::Integrity(
            "invalid local administration state",
        ));
    }
    Ok(())
}
