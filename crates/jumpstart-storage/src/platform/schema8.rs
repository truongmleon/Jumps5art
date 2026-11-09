//! Records local Mass publication intents and immutable chunk hashes.
//!
//! The intent preserves item and version identity across retry. Chunk hashes
//! let a reader verify one range without downloading the rest of the file.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Result, StorageError};

pub(super) const MIGRATION_NAME: &str = "mass_local_publication";

const OPERATIONS: &str = "CREATE TABLE mass_local_operations (
    sequence INTEGER PRIMARY KEY CHECK (sequence>0),
    request_id BLOB NOT NULL UNIQUE CHECK (length(request_id)=16),
    request_digest BLOB NOT NULL CHECK (length(request_digest)=32),
    item_id BLOB NOT NULL UNIQUE CHECK (length(item_id)=16),
    version_id BLOB UNIQUE CHECK (version_id IS NULL OR length(version_id)=16),
    resource_id BLOB UNIQUE CHECK (resource_id IS NULL OR length(resource_id)=16),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms>=0),
    completed INTEGER NOT NULL CHECK (completed IN (0,1)),
    CHECK ((version_id IS NULL)=(resource_id IS NULL))
) STRICT";

const CHUNKS: &str = "CREATE TABLE mass_content_chunks (
    version_id BLOB NOT NULL REFERENCES versions(version_id),
    chunk_index INTEGER NOT NULL CHECK (chunk_index>=0),
    digest BLOB NOT NULL CHECK (length(digest)=32),
    PRIMARY KEY (version_id,chunk_index)
) STRICT";

/// Return the expected SQLite definitions for Mass publication.
pub(super) fn statements() -> [&'static str; 2] {
    [OPERATIONS, CHUNKS]
}

/// Hash the exact definitions recorded in the migration history.
pub(super) fn checksum() -> String {
    let mut hash = blake3::Hasher::new();
    for statement in statements() {
        hash.update(statement.as_bytes());
        hash.update(b";\n");
    }
    format!("b3:{}", hash.finalize())
}

/// Install Mass publication metadata on a checked schema 7 connection.
pub(super) fn apply(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 7 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 7,
        });
    }
    for statement in statements() {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO schema_migrations (version,name,checksum,applied_at)
         VALUES (8,?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        params![MIGRATION_NAME, checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 8)?;
    transaction.commit()?;
    Ok(())
}
