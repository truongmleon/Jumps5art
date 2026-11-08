//! Exercises schema 4 through a real initialized metadata root.
//!
//! These fixtures write only test databases. They verify which relationships
//! SQLite enforces before an authenticated application writer is available.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use jumpstart_storage::{
    DATABASE_DIRECTORY, DATABASE_FILE, SNAPSHOT_DATABASE_FILE, create_snapshot, initialize,
    inspect, inventory_snapshot, verify_snapshot,
};
use rusqlite::{Connection, OpenFlags, Transaction, params};

const PRIMARY: [u8; 16] = [1; 16];
const VOLUME: [u8; 16] = [2; 16];

/// Open a fresh catalog for test inserts, with foreign key enforcement enabled.
///
/// `root` is a new directory. Returns the writable connection, with one
/// registered primary fixture to make placements and owned items possible.
fn open_catalog(root: &Path) -> Connection {
    initialize(root).unwrap();
    let connection = Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    connection
        .execute(
            "INSERT INTO volumes VALUES (?1, 'primary', ?2, 'test-filesystem', 'unknown', NULL)",
            params![VOLUME, PRIMARY],
        )
        .unwrap();
    connection
}

/// A shared object counts once per primary, with missing placements tracked per copy.
#[test]
fn backup_inventory_counts_retained_versions_and_deduplicates_within_each_primary() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("metadata");
    let mut connection = open_catalog(&root);
    let transaction = connection.transaction().unwrap();
    let (photo, photo_version) = insert_versioned_item(&transaction, 10, "photogenic", "Photo");
    let (_, mass_version) = insert_versioned_item(&transaction, 11, "mass", "File");
    let (other, other_version) = insert_versioned_item(&transaction, 12, "mass", "Other primary");
    let second_primary = [3_u8; 16];
    transaction
        .execute(
            "INSERT INTO volumes VALUES (?1, 'primary', ?2, 'second-filesystem', 'unknown', NULL)",
            params![[4_u8; 16], second_primary],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE items SET home_primary_id=?1 WHERE item_id=?2",
            params![second_primary, other],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE items SET lifecycle='recently_deleted', deleted_at_ms=1000 WHERE item_id=?1",
            [photo],
        )
        .unwrap();
    let older_version = [99_u8; 16];
    transaction
        .execute(
            "INSERT INTO versions VALUES (?1, ?2, 1, 1, 'b3', zeroblob(32))",
            params![older_version, photo],
        )
        .unwrap();
    transaction
        .execute_batch("INSERT INTO blobs VALUES ('b3', zeroblob(32), 100);")
        .unwrap();
    for version in [photo_version, older_version, mass_version, other_version] {
        transaction.execute(
            "INSERT INTO version_resources VALUES (?1, zeroblob(16), 'original', 'file', NULL, 'b3', zeroblob(32), 1)",
            [version],
        ).unwrap();
    }
    transaction
        .execute(
            "INSERT INTO blob_placements VALUES ('b3', zeroblob(32), ?1, 'verified', 1000)",
            [PRIMARY],
        )
        .unwrap();
    transaction.execute(
        "INSERT INTO worlds_backup_catalog VALUES (zeroblob(16), ?1, 1, 'b3', zeroblob(32), 1000, 2000)",
        [VOLUME],
    ).unwrap();
    transaction.commit().unwrap();
    connection.close().unwrap();

    let output = parent.path().join("snapshot");
    let snapshot = create_snapshot(&root, &output).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    let inventory = inventory_snapshot(&output).unwrap();
    assert_eq!(inventory.source, snapshot);
    assert_eq!(inventory.worlds_recovery_points.value(), 1);
    assert_eq!(inventory.primaries.len(), 2);
    let first = &inventory.primaries[0];
    assert_eq!(first.primary_id.as_bytes(), &PRIMARY);
    assert_eq!(first.items.value(), 2);
    assert_eq!(first.versions.value(), 3);
    assert_eq!(first.resources.value(), 3);
    assert_eq!(first.unique_blobs.value(), 1);
    assert_eq!(first.blob_bytes.value(), 100);
    assert_eq!(first.blobs_without_verified_placement.value(), 0);
    let second = &inventory.primaries[1];
    assert_eq!(second.primary_id.as_bytes(), &second_primary);
    assert_eq!(second.unique_blobs.value(), 1);
    assert_eq!(second.blob_bytes.value(), 100);
    assert_eq!(second.blobs_without_verified_placement.value(), 1);
    assert_eq!(verify_snapshot(&output).unwrap(), snapshot);
}

/// Insert a catalog item and its first version in a transaction.
///
/// `key` provides distinct test IDs, `owner` selects the app and `name` is the
/// display name. Returns the item and version IDs for additional records.
fn insert_versioned_item(
    transaction: &Transaction<'_>,
    key: u8,
    owner: &str,
    name: &str,
) -> ([u8; 16], [u8; 16]) {
    let item = [key; 16];
    let version = [key + 100; 16];
    transaction
        .execute(
            "INSERT INTO items VALUES (?1, ?1, ?2, 'fixture', ?3, NULL, ?4, 'live', NULL, ?5, 1)",
            params![item, owner, PRIMARY, name, version],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO versions VALUES (?1, ?2, ?3, 1, 'b3', zeroblob(32))",
            params![version, item, key],
        )
        .unwrap();
    (item, version)
}

/// Keep app-specific state linked to a single catalog, with shared immutable bytes.
#[test]
fn all_five_owners_and_worlds_can_store_their_catalog_records() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("metadata");
    let mut connection = open_catalog(&root);
    let transaction = connection.transaction().unwrap();
    let (track, audio_version) = insert_versioned_item(&transaction, 10, "muse", "Track");
    let (_, photo_version) = insert_versioned_item(&transaction, 11, "photogenic", "Live Photo");
    let (_, file_version) = insert_versioned_item(&transaction, 12, "mass", "Original.heic");
    let (book, chapter_version) = insert_versioned_item(&transaction, 13, "bato", "Chapter");
    let (episode, episode_version) = insert_versioned_item(&transaction, 14, "flex", "Episode");

    transaction
        .execute(
            "INSERT INTO music_track VALUES (?1, 'muse', ?2, 240000, 'aac', 1, 'b3', zeroblob(32))",
            params![track, audio_version],
        )
        .unwrap();
    transaction
        .execute_batch(
            "INSERT INTO blobs VALUES ('b3', zeroblob(32), 100), ('b3', randomblob(32), 800);",
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO version_resources
             SELECT ?1, zeroblob(16), 'original_still', 'Original.heic', 'image/heic', algorithm, digest, 1
             FROM blobs WHERE byte_length=100 UNION ALL
             SELECT ?1, randomblob(16), 'paired_motion', 'Original.mov', 'video/quicktime', algorithm, digest, 1
             FROM blobs WHERE byte_length=800 UNION ALL
             SELECT ?2, randomblob(16), 'original', 'Original.heic', 'image/heic', algorithm, digest, 1
             FROM blobs WHERE byte_length=100",
            params![photo_version, file_version],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO chapter_releases VALUES
             (zeroblob(16), ?1, 'bato', ?2, 'fixture', 'series', 'release', 'en', NULL, NULL, 'Prologue')",
            params![book, chapter_version],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO bookmarks VALUES (zeroblob(16), ?1, ?2, 1, '{}', 1)",
            params![book, chapter_version],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO media_marker VALUES (?1, 'flex', ?2, 'ending', 500, 1000, 'manual', 1, 0, 0)",
            params![episode, episode_version],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO worlds_backup_catalog VALUES (zeroblob(16), ?1, 1, 'b3', zeroblob(32), 1000, 2000)",
            [VOLUME],
        )
        .unwrap();
    let playlist = [50_u8; 16];
    transaction
        .execute(
            "INSERT INTO collections VALUES (?1, 'muse', 'Playlist', 1)",
            [playlist],
        )
        .unwrap();
    for position in [0_u8, 1] {
        transaction
            .execute(
                "INSERT INTO collection_entries VALUES (?1, ?2, 'muse', ?3, ?4)",
                params![[position; 16], playlist, track, position],
            )
            .unwrap();
    }
    transaction.commit().unwrap();

    connection
        .execute(
            "DELETE FROM collection_entries WHERE collection_id=?1",
            [playlist],
        )
        .unwrap();
    let tracks: i64 = connection
        .query_row("SELECT count(*) FROM music_track", [], |row| row.get(0))
        .unwrap();
    assert_eq!(tracks, 1);
    let still_references: i64 = connection
        .query_row(
            "SELECT count(*) FROM version_resources WHERE digest=zeroblob(32)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(still_references, 2);
    assert!(connection.execute("DELETE FROM blobs", []).is_err());
    connection.close().unwrap();
    inspect(&root).unwrap();

    let output = parent.path().join("snapshot");
    let snapshot = create_snapshot(&root, &output).unwrap();
    assert_eq!(verify_snapshot(&output).unwrap(), snapshot);
    let copy = Connection::open_with_flags(
        output.join(SNAPSHOT_DATABASE_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let item_count: i64 = copy
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(item_count, 5);
}

/// Reject cross-app parents and cross-item versions at the transaction boundary.
/// Enforce live Mass names at the root while allowing retained names and other apps.
#[test]
fn catalog_rejects_wrong_ownership_version_and_mass_name_collisions() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("metadata");
    let mut connection = open_catalog(&root);
    let transaction = connection.transaction().unwrap();
    let (file, _) = insert_versioned_item(&transaction, 10, "mass", "Same");
    let (photo, photo_version) = insert_versioned_item(&transaction, 11, "photogenic", "Same");
    transaction.commit().unwrap();

    let insert =
        "INSERT INTO items VALUES (?1, ?1, 'mass', 'file', ?2, NULL, 'Same', ?3, ?4, NULL, 1)";
    assert!(
        connection
            .execute(insert, params![[12_u8; 16], PRIMARY, "live", None::<i64>])
            .is_err()
    );
    assert!(
        connection
            .execute(
                insert,
                params![[12_u8; 16], PRIMARY, "recently_deleted", 1000]
            )
            .is_ok()
    );

    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE items SET parent_id=?1 WHERE item_id=?2",
            params![photo, file],
        )
        .unwrap();
    assert!(transaction.commit().is_err());

    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE items SET current_version_id=?1 WHERE item_id=?2",
            params![photo_version, file],
        )
        .unwrap();
    assert!(transaction.commit().is_err());

    assert!(
        connection
            .execute(
                "INSERT INTO music_track VALUES (?1, 'muse', ?2, 1000, 'aac', 1, 'b3', zeroblob(32))",
                params![photo, photo_version],
            )
            .is_err()
    );
}

/// Keep the minimal camera suppression record even when all original bytes are absent.
#[test]
fn camera_suppression_survives_without_catalog_items_or_blobs() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("metadata");
    let connection = open_catalog(&root);
    connection
        .execute_batch(
            "INSERT INTO deletion_tombstones VALUES (zeroblob(16), zeroblob(16), 1);
             INSERT INTO camera_suppression VALUES (1, 'b3', zeroblob(32), zeroblob(16), zeroblob(16));
             INSERT INTO camera_suppression_resources VALUES
             (1, 'b3', zeroblob(32), zeroblob(16), zeroblob(16), 'original_still', 'b3', zeroblob(32), 100, 2);",
        )
        .unwrap();
    assert!(
        connection
            .execute("DELETE FROM deletion_tombstones", [])
            .is_err()
    );
    connection.close().unwrap();
    inspect(&root).unwrap();
}
