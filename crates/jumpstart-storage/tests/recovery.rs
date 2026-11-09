//! Exercises clean-root recovery using real snapshot bundles and SQLite state.
//!
//! A snapshot taken while a job is running must not schedule that old job on
//! the recovered authority. Tests also protect occupied paths and old schemas.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::fs;

use jumpstart_domain::BlobId;
use jumpstart_storage::{
    DATABASE_DIRECTORY, DATABASE_FILE, SNAPSHOT_DATABASE_FILE, StorageError, create_snapshot,
    enqueue_snapshot_job, initialize, inspect, list_jobs, pull_job_changes,
    restore_metadata_snapshot, run_snapshot_job, verify_snapshot,
};

/// Recover from a job's snapshot after the original installation disappears.
#[test]
fn recovery_creates_new_authority_without_replaying_snapshotted_work() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("source");
    let backup = parent.path().join("backup");
    let output = parent.path().join("recovered");
    let old = initialize(&root).unwrap();
    let active = enqueue_snapshot_job(&root, &backup).unwrap();
    enqueue_snapshot_job(&root, &parent.path().join("old-queued-output")).unwrap();
    let completed = run_snapshot_job(&root, active.job_id).unwrap();
    let saved = completed.result.unwrap();
    fs::remove_dir_all(&root).unwrap();

    let recovered = restore_metadata_snapshot(&backup, &output, saved.database_hash).unwrap();
    assert_eq!(recovered.source, saved);
    assert_ne!(recovered.metadata.server_id, old.server_id);
    assert_ne!(recovered.metadata.sync_epoch, old.sync_epoch);
    assert_eq!(recovered.metadata.change_seq, saved.change_seq);
    assert_eq!(recovered.archived_snapshot_jobs.value(), 2);
    assert_eq!(inspect(&output).unwrap(), recovered.metadata);
    assert!(list_jobs(&output, 10).unwrap().is_empty());
    assert!(!parent.path().join("old-queued-output").exists());

    let after = saved.change_seq.unwrap();
    assert!(matches!(
        pull_job_changes(&output, old.sync_epoch, after, 10),
        Err(StorageError::JobChangeEpochMismatch { .. })
    ));
    let page = pull_job_changes(&output, recovered.metadata.sync_epoch, after, 10).unwrap();
    assert!(page.batches.is_empty());
    let queued = enqueue_snapshot_job(&output, &parent.path().join("new-output")).unwrap();
    assert_eq!(queued.created_seq.value(), after.value() + 1);
    assert_eq!(verify_snapshot(&backup).unwrap(), saved);
}

/// Require the inspected digest and keep existing destinations and backups intact.
#[test]
fn recovery_refuses_conflicting_paths_and_changed_backup_bytes() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("source");
    let backup = parent.path().join("backup");
    let output = parent.path().join("recovered");
    initialize(&root).unwrap();
    let saved = create_snapshot(&root, &backup).unwrap();

    assert!(matches!(
        restore_metadata_snapshot(&backup, &output, BlobId::hash_bytes(b"wrong snapshot")),
        Err(StorageError::InvalidMetadataRestore(_))
    ));
    assert!(!output.exists());
    assert!(matches!(
        restore_metadata_snapshot(&backup, &backup.join("nested"), saved.database_hash),
        Err(StorageError::InvalidMetadataRestore(_))
    ));
    assert!(matches!(
        restore_metadata_snapshot(&backup, &root.join("nested"), saved.database_hash),
        Err(StorageError::InvalidMetadataRestore(_))
    ));
    fs::create_dir(&output).unwrap();
    fs::write(output.join("keep"), b"existing data").unwrap();
    assert!(matches!(
        restore_metadata_snapshot(&backup, &output, saved.database_hash),
        Err(StorageError::ExistingTarget(_))
    ));
    assert_eq!(fs::read(output.join("keep")).unwrap(), b"existing data");
    assert_eq!(verify_snapshot(&backup).unwrap(), saved);

    let absent = parent.path().join("no-publication");
    fs::write(backup.join(SNAPSHOT_DATABASE_FILE), b"damaged").unwrap();
    assert!(restore_metadata_snapshot(&backup, &absent, saved.database_hash).is_err());
    assert!(!absent.exists());
}

/// Older standalone snapshots retain the correct schema and watermark on recovery.
#[test]
fn recovery_accepts_older_metadata_without_inventing_a_watermark() {
    for (version, create) in [
        (1, support::create_schema1 as fn(&std::path::Path) -> _),
        (2, support::create_schema2),
    ] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("old");
        let backup = parent.path().join("backup");
        let output = parent.path().join("recovered");
        let old = create(&root);
        let saved = create_snapshot(&root, &backup).unwrap();
        let recovered = restore_metadata_snapshot(&backup, &output, saved.database_hash).unwrap();
        assert_eq!(recovered.metadata.schema_version, version);
        assert_eq!(recovered.metadata.change_seq, old.change_seq);
        assert_ne!(recovered.metadata.sync_epoch, old.sync_epoch);
        assert_eq!(recovered.archived_snapshot_jobs.value(), 0);
        assert_eq!(inspect(&output).unwrap(), recovered.metadata);
    }
}

/// A missing primary cannot be hidden by publishing only its catalog database.
#[test]
fn recovery_requires_complete_generation_for_registered_storage() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("source");
    let backup = parent.path().join("backup");
    let output = parent.path().join("recovered");
    initialize(&root).unwrap();
    let database =
        rusqlite::Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
    database
        .execute(
            "INSERT INTO volumes(volume_id,role,primary_id,filesystem_identity,health)
         VALUES (zeroblob(16),'primary',zeroblob(16),'fixture','unknown')",
            [],
        )
        .unwrap();
    database.close().unwrap();
    let saved = create_snapshot(&root, &backup).unwrap();
    let error = restore_metadata_snapshot(&backup, &output, saved.database_hash).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("registered storage requires a complete recovery generation")
    );
    assert!(!output.exists());
    assert_eq!(verify_snapshot(&backup).unwrap(), saved);
}
