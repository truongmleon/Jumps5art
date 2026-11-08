//! Inventories the objects a backup needs from a verified catalog snapshot.
//!
//! A track and another item may reference the same bytes on one primary. This
//! reports both logical resource counts and the deduplicated physical work.
//! It only uses the snapshot's SQLite data, even if a primary is offline.

use std::path::Path;

use jumpstart_domain::Counter;
use uuid::Uuid;

use super::snapshot;
use crate::{PrimaryBackupInventory, Result, SnapshotBackupInventory, StorageError};

const INVENTORY_SQL: &str = "
WITH resources AS (
    SELECT i.home_primary_id AS primary_id, r.algorithm, r.digest
    FROM items AS i
    JOIN versions AS v ON v.item_id = i.item_id
    JOIN version_resources AS r ON r.version_id = v.version_id
),
unique_objects AS (
    SELECT DISTINCT primary_id, algorithm, digest FROM resources
),
object_counts AS (
    SELECT o.primary_id, count(*) AS objects, sum(b.byte_length) AS bytes,
           count(*) FILTER (WHERE p.state IS NOT 'verified') AS without_verified_placement
    FROM unique_objects AS o
    JOIN blobs AS b USING (algorithm, digest)
    LEFT JOIN blob_placements AS p USING (primary_id, algorithm, digest)
    GROUP BY o.primary_id
),
resource_counts AS (
    SELECT primary_id, count(*) AS resources FROM resources GROUP BY primary_id
)
SELECT v.primary_id,
       (SELECT count(*) FROM items AS i WHERE i.home_primary_id = v.primary_id),
       (SELECT count(*) FROM versions AS ver JOIN items AS i USING (item_id)
        WHERE i.home_primary_id = v.primary_id),
       coalesce(r.resources, 0),
       coalesce(o.objects, 0),
       coalesce(o.bytes, 0),
       coalesce(o.without_verified_placement, 0)
FROM volumes AS v
LEFT JOIN object_counts AS o USING (primary_id)
LEFT JOIN resource_counts AS r USING (primary_id)
WHERE v.role = 'primary'
ORDER BY v.primary_id";

/// Read one independently verified catalog snapshot's backup inventory.
///
/// `root` selects the snapshot, not the original metadata directory. Returns
/// exact counts across all retained versions, including Recently Deleted items.
/// An old schema is reported explicitly because it cannot supply a catalog.
pub(crate) fn inventory_snapshot(root: &Path) -> Result<SnapshotBackupInventory> {
    let (source, connection) = snapshot::open_verified_snapshot(root)?;
    if source.schema_version < 4 {
        return Err(StorageError::BackupCatalogUnavailable {
            found: source.schema_version,
        });
    }
    let primaries = {
        let mut statement = connection.prepare(INVENTORY_SQL)?;
        let mut rows = statement.query([])?;
        let mut primaries = Vec::new();
        while let Some(row) = rows.next()? {
            let id: Vec<u8> = row.get(0)?;
            let primary_id = Uuid::from_slice(&id).map_err(|_| {
                StorageError::Integrity("invalid primary identity in backup catalog")
            })?;
            primaries.push(PrimaryBackupInventory {
                primary_id,
                items: count(row.get(1)?)?,
                versions: count(row.get(2)?)?,
                resources: count(row.get(3)?)?,
                unique_blobs: count(row.get(4)?)?,
                blob_bytes: count(row.get(5)?)?,
                blobs_without_verified_placement: count(row.get(6)?)?,
            });
        }
        primaries
    };
    let worlds_recovery_points = count(connection.query_row(
        "SELECT count(*) FROM worlds_backup_catalog",
        [],
        |row| row.get(0),
    )?)?;
    connection
        .close()
        .map_err(|(_, error)| StorageError::Database(error))?;
    Ok(SnapshotBackupInventory {
        source,
        primaries,
        worlds_recovery_points,
    })
}

/// Convert a nonnegative SQLite count into its exact wire representation.
fn count(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::Integrity("invalid backup inventory count"))
}
