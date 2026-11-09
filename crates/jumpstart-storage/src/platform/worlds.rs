//! Reads the Worlds index from the daemon's checked SQLite lease.
//!
//! The catalog can show that a recovery point was recorded. The independent
//! filesystem manifest and its objects are still needed to verify or restore it.

use jumpstart_domain::{BlobId, Counter};
use rusqlite::Connection;
use serde::Serialize;
use uuid::Uuid;

use super::AdminMetadata;
use crate::{MAX_WORLDS_INDEX_ROWS, Result, StorageError};

/// Catalog evidence for Worlds status, before a live Minecraft adapter exists.
#[derive(Serialize)]
pub struct WorldsCatalogStatus {
    pub registered_primaries: Counter,
    pub indexed_recovery_points: Counter,
}

/// A saved index row identifying a Worlds recovery manifest and its last check.
#[derive(Serialize)]
pub struct WorldsRecoveryPoint {
    pub point_id: Uuid,
    pub volume_id: Uuid,
    pub manifest_version: u32,
    pub manifest_hash: BlobId,
    pub captured_at_ms: i64,
    pub verified_at_ms: i64,
}

/// Bounded recorded recovery points, newest capture first.
///
/// `truncated` tells the CLI that older records exist beyond this result.
#[derive(Serialize)]
pub struct WorldsRecoveryIndex {
    pub points: Vec<WorldsRecoveryPoint>,
    pub truncated: bool,
}

impl AdminMetadata {
    /// Read counts from the checked catalog for a protected Worlds status request.
    ///
    /// Returns the number of registered primaries and indexed recovery points.
    /// Older schemas or a changed metadata binding return an error.
    pub fn worlds_catalog_status(&self) -> Result<WorldsCatalogStatus> {
        let connection = self.worlds_catalog()?;
        let (primaries, points): (i64, i64) = connection.query_row(
            "SELECT (SELECT count(*) FROM volumes WHERE role='primary'),
                    (SELECT count(*) FROM worlds_backup_catalog)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(WorldsCatalogStatus {
            registered_primaries: count(primaries)?,
            indexed_recovery_points: count(points)?,
        })
    }

    /// Return up to `limit` indexed Worlds points in a stable order.
    ///
    /// Times are captured and verified UTC Unix milliseconds from SQLite. One
    /// extra row detects truncation without reading the rest of the index into
    /// memory. This never reads a world file or updates backup history.
    pub fn worlds_recovery_index(&self, limit: u16) -> Result<WorldsRecoveryIndex> {
        if !(1..=MAX_WORLDS_INDEX_ROWS).contains(&limit) {
            return Err(StorageError::WorldsCatalogUnavailable(
                "invalid Worlds index limit",
            ));
        }
        let connection = self.worlds_catalog()?;
        let mut statement = connection.prepare(
            "SELECT point_id,volume_id,manifest_version,manifest_digest,captured_at_ms,verified_at_ms
             FROM worlds_backup_catalog ORDER BY captured_at_ms DESC,point_id LIMIT ?1",
        )?;
        let mut rows = statement.query([i64::from(limit) + 1])?;
        let mut points = Vec::new();
        while points.len() < usize::from(limit) {
            let Some(row) = rows.next()? else { break };
            let point: Vec<u8> = row.get(0)?;
            let volume: Vec<u8> = row.get(1)?;
            let digest: Vec<u8> = row.get(3)?;
            points.push(WorldsRecoveryPoint {
                point_id: uuid(&point)?,
                volume_id: uuid(&volume)?,
                manifest_version: row.get(2)?,
                manifest_hash: BlobId::from_bytes(digest.try_into().map_err(|_| {
                    StorageError::Integrity("invalid Worlds recovery manifest digest")
                })?),
                captured_at_ms: row.get(4)?,
                verified_at_ms: row.get(5)?,
            });
        }
        Ok(WorldsRecoveryIndex {
            points,
            truncated: rows.next()?.is_some(),
        })
    }

    /// Borrow the held SQLite view if its schema and path still support Worlds.
    fn worlds_catalog(&self) -> Result<&Connection> {
        self.check_binding()?;
        if self.report().schema_version < 4 {
            return Err(StorageError::WorldsCatalogUnavailable(
                "upgrade metadata to the application catalog before inspecting Worlds",
            ));
        }
        Ok(&self.source.connection)
    }
}

/// Convert a checked catalog count to an exact JSON counter.
fn count(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::Integrity("invalid Worlds catalog count"))
}

/// Read a UUID stored as a 16-byte SQLite value.
fn uuid(bytes: &[u8]) -> Result<Uuid> {
    Uuid::from_slice(bytes).map_err(|_| StorageError::Integrity("invalid Worlds index identity"))
}
