//! Installs the shared application catalog into the server's existing SQLite file.
//!
//! Items carry logical ownership. Versions describe immutable content, and a blob
//! identifies bytes that different versions can share. Keeping those identities
//! separate lets removing a playlist entry or photo album reference leave its
//! library item and original files alone.
//!
//! Once deployed, the SQL source must remain byte-for-byte stable because the
//! migration record stores its checksum. Evolve it through a later migration.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Result, StorageError};

const CATALOG_SQL: &str = include_str!("schema4.sql");
pub(super) const MIGRATION_NAME: &str = "shared_application_catalog";

/// Return the exact table and index definitions expected in a checked catalog.
///
/// The embedded SQL uses a header followed by simple DDL statements, each ending
/// in a semicolon and newline. Returns definitions without those separators,
/// matching SQLite's stored definitions.
pub(super) fn statements() -> impl Iterator<Item = &'static str> {
    let (_, ddl) = CATALOG_SQL
        .split_once("\n\n")
        .expect("catalog migration has a header and definitions");
    ddl.split_terminator(";\n").map(str::trim)
}

/// Return the tagged BLAKE3 checksum of the frozen catalog migration source.
pub(super) fn checksum() -> String {
    format!("b3:{}", blake3::hash(CATALOG_SQL.as_bytes()))
}

/// Install an empty catalog without rewriting any existing job or authority row.
///
/// `connection` is the checked metadata writer under exclusive maintenance
/// ownership. Returns `Ok(())` after all definitions, their migration record,
/// and SQLite's version commit together. A wrong source version or a failed SQL
/// statement leaves the transaction uncommitted.
pub(super) fn apply(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 3 {
        return Err(StorageError::UnsupportedSchema {
            found: version,
            supported: 3,
        });
    }
    transaction.execute_batch(CATALOG_SQL)?;
    transaction.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (4, ?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![MIGRATION_NAME, checksum()],
    )?;
    transaction.pragma_update(None, "user_version", 4)?;
    transaction.commit()?;
    Ok(())
}
