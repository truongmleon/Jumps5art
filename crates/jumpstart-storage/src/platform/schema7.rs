//! Adds durable device authority and each client's independent sequence boundary.
//!
//! A revoked key stays in this catalog so a later pairing cannot re-authorize
//! it. The first client database is saved in the same transaction as approval.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Result, StorageError};

pub(super) const MIGRATION_NAME: &str = "device_authority";

const DEVICES: &str = "CREATE TABLE devices (
    device_id BLOB PRIMARY KEY CHECK (length(device_id)=16),
    server_id BLOB NOT NULL CHECK (length(server_id)=16),
    public_key BLOB NOT NULL UNIQUE CHECK (length(public_key)=32),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    platform TEXT NOT NULL CHECK (length(platform) BETWEEN 1 AND 32),
    app TEXT NOT NULL CHECK (app IN ('muse','photogenic','mass','bato','flex')),
    approved_at_ms INTEGER NOT NULL CHECK (approved_at_ms>=0),
    last_seen_at_ms INTEGER CHECK (last_seen_at_ms>=0),
    revoked_at_ms INTEGER CHECK (revoked_at_ms>=0),
    pairing_receipt BLOB NOT NULL CHECK (length(pairing_receipt) BETWEEN 1 AND 4096)
) STRICT";

const INSTANCES: &str = "CREATE TABLE client_instances (
    instance_id BLOB PRIMARY KEY CHECK (length(instance_id)=16),
    device_id BLOB NOT NULL REFERENCES devices(device_id),
    next_expected INTEGER NOT NULL CHECK (next_expected>=1),
    last_acked_cursor INTEGER NOT NULL CHECK (last_acked_cursor>=0)
) STRICT";

const INSTANCE_DEVICE: &str = "CREATE INDEX client_instances_device ON client_instances(device_id)";

/// Return the exact definitions expected in the authority catalog.
pub(super) fn statements() -> [&'static str; 3] {
    [DEVICES, INSTANCES, INSTANCE_DEVICE]
}

/// Fingerprint the definitions applied by this migration.
pub(super) fn checksum() -> String {
    let mut hasher = blake3::Hasher::new();
    for statement in statements() {
        hasher.update(statement.as_bytes());
        hasher.update(b";\n");
    }
    format!("b3:{}", hasher.finalize())
}

/// Add the authority tables transactionally to a checked schema 6 database.
pub(super) fn apply(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 6 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 6,
        });
    }
    for statement in statements() {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO schema_migrations (version,name,checksum,applied_at)
         VALUES (7,?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        params![MIGRATION_NAME, checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 7)?;
    transaction.commit()?;
    Ok(())
}

/// Require every persisted approval to belong to the current server authority.
pub(super) fn verify_state(connection: &Connection) -> Result<()> {
    let valid: bool = connection.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM devices AS d
             WHERE d.server_id != (SELECT server_id FROM server_state WHERE singleton=1)
                OR NOT EXISTS (SELECT 1 FROM client_instances AS c WHERE c.device_id=d.device_id)
         )",
        [],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(StorageError::Integrity("invalid device authority"));
    }
    Ok(())
}
