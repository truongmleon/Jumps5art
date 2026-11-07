//! Frozen schema-1 fixtures for compatibility and explicit-upgrade tests.
//!
//! Creating old data through today's initializer would silently change the fixture
//! whenever initialization advances. These SQL strings, marker fields, and migration
//! checksum inputs preserve the original two-table schema independently instead.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use jumpstart_domain::{ServerId, SyncEpoch};
use jumpstart_storage::{MetadataReport, inspect};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

const ORIGINAL_MIGRATIONS: &str = "CREATE TABLE schema_migrations (\n    version INTEGER PRIMARY KEY CHECK (version > 0),\n    name TEXT NOT NULL,\n    checksum TEXT NOT NULL,\n    applied_at TEXT NOT NULL\n) STRICT";
const ORIGINAL_STATE: &str = "CREATE TABLE server_state (\n    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n    format_version INTEGER NOT NULL CHECK (format_version > 0),\n    server_id BLOB NOT NULL CHECK (length(server_id) = 16),\n    sync_epoch BLOB NOT NULL CHECK (length(sync_epoch) = 16)\n) STRICT";

/// Create a genuine original schema-1 root without using the current initializer.
///
/// # Arguments
/// * `root` — `&Path` of a new test directory whose parent already exists.
///
/// # Returns
/// A `MetadataReport` obtained by inspecting the independently created fixture.
/// Its schema version is one and its change sequence is unavailable.
///
/// # Panics
/// Panics on fixture creation, SQLite, synchronization, or inspection failures.
pub fn create_schema1(root: &Path) -> MetadataReport {
    fs::DirBuilder::new().mode(0o700).create(root).unwrap();
    for directory in ["database", "secrets", "diagnostics", "snapshots"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join(directory))
            .unwrap();
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("metadata.lock"))
        .unwrap();
    lock.sync_all().unwrap();
    let server_id = ServerId::new();
    let sync_epoch = SyncEpoch::new();
    let marker = serde_json::json!({
        "format_version": 1,
        "schema_version": 1,
        "server_id": server_id,
        "sync_epoch": sync_epoch,
    });
    fs::write(
        root.join("format.json"),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    let path = root.join("database/jumpstart.sqlite");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    transaction.execute_batch(ORIGINAL_MIGRATIONS).unwrap();
    transaction.execute_batch(ORIGINAL_STATE).unwrap();
    let mut checksum = blake3::Hasher::new();
    checksum.update(ORIGINAL_MIGRATIONS.as_bytes());
    checksum.update(b";\n");
    checksum.update(ORIGINAL_STATE.as_bytes());
    checksum.update(b";\n");
    transaction.execute(
        "INSERT INTO schema_migrations VALUES (1, 'initial_metadata', ?1, '2026-10-26T00:00:00.000Z')",
        [format!("b3:{}", checksum.finalize().to_hex())],
    ).unwrap();
    transaction
        .execute(
            "INSERT INTO server_state VALUES (1, 1, ?1, ?2)",
            params![
                server_id.as_uuid().as_bytes().as_slice(),
                sync_epoch.as_uuid().as_bytes().as_slice()
            ],
        )
        .unwrap();
    transaction
        .pragma_update(None, "application_id", 0x4a53_4d44)
        .unwrap();
    transaction.pragma_update(None, "user_version", 1).unwrap();
    transaction.commit().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    connection.close().unwrap();
    file.sync_all().unwrap();
    inspect(root).expect("original schema-1 fixture must remain readable")
}

/// Create the frozen schema-2 layout without following today's initializer.
///
/// `root` names a new test directory. Returns `MetadataReport` after applying the
/// original watermark SQL/checksum to the independent schema-1 fixture. Panics on
/// fixture errors; this keeps old-schema tests meaningful when new init advances.
pub fn create_schema2(root: &Path) -> MetadataReport {
    create_schema1(root);
    let path = root.join("database/jumpstart.sqlite");
    let mut connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=0;",
        )
        .unwrap();
    let original_sql = "ALTER TABLE server_state ADD COLUMN change_seq INTEGER NOT NULL DEFAULT 0 CHECK (change_seq >= 0)";
    let mut checksum = blake3::Hasher::new();
    checksum.update(original_sql.as_bytes());
    checksum.update(b";\n");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    transaction.execute_batch(original_sql).unwrap();
    transaction.execute(
        "INSERT INTO schema_migrations VALUES (2, 'change_watermark', ?1, '2026-10-27T00:00:00.000Z')",
        [format!("b3:{}", checksum.finalize().to_hex())],
    ).unwrap();
    transaction.pragma_update(None, "user_version", 2).unwrap();
    transaction.commit().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    connection.close().unwrap();
    let mut marker: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("format.json")).unwrap()).unwrap();
    marker["schema_version"] = 2.into();
    fs::write(
        root.join("format.json"),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    inspect(root).expect("frozen schema-2 fixture must remain readable")
}
