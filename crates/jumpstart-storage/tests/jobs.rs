//! Checks that accepted snapshot work and its outcomes survive a command restart.
//!
//! A queued request is durable before its copy begins. Its completed status must
//! follow actual publication and verification, while a failed or canceled request
//! keeps its earlier data and must never overwrite somebody else's destination.
//! Repeating a completed run returns the same saved record, including when its
//! artifact has moved, so a lost success reply does not start a second copy.
//! These tests exercise SQLite records and real scratch snapshots, not a daemon.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::fs;
    use std::path::Path;

    use fs2::FileExt;
    use jumpstart_domain::{Counter, JobId, JobState, JobTransitionError, PositiveCounter};
    use jumpstart_storage::{
        DATABASE_DIRECTORY, DATABASE_FILE, LOCK_FILE, MIGRATION_FILE, SnapshotScope, StorageError,
        cancel_job, enqueue_snapshot_job, initialize, inspect, inspect_job, list_jobs,
        list_jobs_in_state, recover_jobs, run_snapshot_job, verify_snapshot,
    };
    use rusqlite::{Connection, params};

    /// Read the committed global position and number of full job projections.
    ///
    /// `root` is a checkpointed scratch metadata directory. Returns `(u64, u64)`
    /// for the stored high-water mark and batch count; fixture/query failures panic.
    fn positions(root: &Path) -> (u64, u64) {
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        let values = connection.query_row(
            "SELECT change_seq, (SELECT count(*) FROM change_batches) FROM server_state WHERE singleton=1",
            [], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)),
        ).unwrap();
        connection.close().unwrap();
        (
            u64::try_from(values.0).unwrap(),
            u64::try_from(values.1).unwrap(),
        )
    }

    /// Accept and reopen independent requests without executing them or trusting clocks.
    #[test]
    fn queued_jobs_reopen_with_exact_ids_counters_and_stable_order() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let first_output = parent.path().join("first-snapshot");
        let first = enqueue_snapshot_job(&root, &first_output).unwrap();
        let second = enqueue_snapshot_job(&root, &parent.path().join("second-snapshot")).unwrap();
        assert_ne!(first.job_id, second.job_id);
        assert_eq!(first.state, JobState::Queued);
        assert_eq!(first.revision, PositiveCounter::ONE);
        assert_eq!(first.attempt_generation, Counter::ZERO);
        assert_eq!(first.created_seq.value(), 1);
        assert_eq!(second.created_seq.value(), 2);
        assert!(first.result.is_none() && first.error.is_none());
        assert_eq!(inspect_job(&root, first.job_id).unwrap(), first);
        assert_eq!(list_jobs(&root, 1).unwrap(), vec![second.clone()]);
        assert_eq!(list_jobs(&root, 100).unwrap(), vec![second, first.clone()]);
        assert_eq!(positions(&root), (2, 2));
        assert!(!first_output.exists());
        let json = serde_json::to_value(&first).unwrap();
        assert_eq!(json["state"], "queued");
        assert_eq!(json["revision"], "1");
        assert_eq!(json["attempt_generation"], "0");
        assert_eq!(json["created_seq"], "1");
    }

    /// Older queued work must remain discoverable even when newer outcomes occupy
    /// an unfiltered page. Filtering and listing must preserve all saved data.
    #[test]
    fn state_filter_precedes_limit_and_keeps_acceptance_order_without_mutation() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let oldest_output = parent.path().join("oldest-queued");
        let oldest = enqueue_snapshot_job(&root, &oldest_output).unwrap();
        let next_output = parent.path().join("next-queued");
        let next = enqueue_snapshot_job(&root, &next_output).unwrap();
        let completed = enqueue_snapshot_job(&root, &parent.path().join("completed-copy")).unwrap();
        let completed = run_snapshot_job(&root, completed.job_id).unwrap();
        let failed_output = parent.path().join("occupied-copy");
        let failed = enqueue_snapshot_job(&root, &failed_output).unwrap();
        fs::create_dir(&failed_output).unwrap();
        fs::write(failed_output.join("keep.txt"), b"keep occupied output").unwrap();
        let failed = run_snapshot_job(&root, failed.job_id).unwrap();
        assert_eq!(
            list_jobs(&root, 2).unwrap(),
            vec![failed.clone(), completed.clone()]
        );
        let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
        let database_before = fs::read(&database).unwrap();
        let marker_before = fs::read(root.join("format.json")).unwrap();
        let metadata_before = inspect(&root).unwrap();

        assert_eq!(
            list_jobs_in_state(&root, JobState::Queued, 1).unwrap(),
            vec![next.clone()]
        );
        assert_eq!(
            list_jobs_in_state(&root, JobState::Queued, 100).unwrap(),
            vec![next, oldest]
        );
        assert_eq!(
            list_jobs_in_state(&root, JobState::Completed, 1).unwrap(),
            vec![completed]
        );
        assert_eq!(
            list_jobs_in_state(&root, JobState::Failed, 1).unwrap(),
            vec![failed]
        );
        for state in [JobState::Running, JobState::Verifying, JobState::Canceled] {
            assert!(list_jobs_in_state(&root, state, 100).unwrap().is_empty());
        }
        assert_eq!(fs::read(&database).unwrap(), database_before);
        assert_eq!(fs::read(root.join("format.json")).unwrap(), marker_before);
        assert_eq!(inspect(&root).unwrap(), metadata_before);
        assert!(!oldest_output.exists());
        assert!(!next_output.exists());
        assert_eq!(
            fs::read(failed_output.join("keep.txt")).unwrap(),
            b"keep occupied output"
        );
    }

    /// Invalid filters/budgets fail before missing-path access, while valid filters
    /// retain the explicit old-schema refusal rather than silently creating a queue.
    #[test]
    fn invalid_filters_missing_paths_and_old_schemas_are_refused_unchanged() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        for state in [
            JobState::Paused,
            JobState::WaitingForSource,
            JobState::WaitingForStorage,
            JobState::WaitingForInput,
        ] {
            assert!(matches!(
                list_jobs_in_state(&missing, state, 1),
                Err(StorageError::InvalidJob(_))
            ));
        }
        for limit in [0, 101, u16::MAX] {
            assert!(matches!(
                list_jobs_in_state(&missing, JobState::Queued, limit),
                Err(StorageError::InvalidJob(_))
            ));
        }
        assert!(list_jobs_in_state(&missing, JobState::Queued, 1).is_err());
        assert!(!missing.exists());
        let one = parent.path().join("schema-one");
        let two = parent.path().join("schema-two");
        super::support::create_schema1(&one);
        super::support::create_schema2(&two);
        for (root, schema) in [(&one, 1), (&two, 2)] {
            let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
            let before = fs::read(&database).unwrap();
            let marker = fs::read(root.join("format.json")).unwrap();
            assert!(
                matches!(list_jobs_in_state(root, JobState::Queued, 1), Err(StorageError::JobsUnavailable { found }) if found == schema)
            );
            assert_eq!(fs::read(database).unwrap(), before);
            assert_eq!(fs::read(root.join("format.json")).unwrap(), marker);
            assert_eq!(inspect(root).unwrap().schema_version, schema);
        }
    }

    /// Complete only after a real snapshot exists and verifies independently.
    #[test]
    fn successful_run_persists_completion_and_the_captured_position() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let output = parent.path().join("snapshot");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &output).unwrap();
        let completed = run_snapshot_job(&root, queued.job_id).unwrap();
        assert_eq!(completed.state, JobState::Completed);
        assert_eq!(completed.revision.value(), 4);
        assert_eq!(completed.attempt_generation.value(), 2);
        assert!(completed.error.is_none());
        let result = completed.result.clone().unwrap();
        assert_eq!(result.scope, SnapshotScope::Metadata);
        assert_eq!(result.change_seq.unwrap().value(), 2);
        assert_eq!(verify_snapshot(&output).unwrap(), result);
        assert_eq!(positions(&root), (4, 4));
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), completed);
        assert_eq!(run_snapshot_job(&root, queued.job_id).unwrap(), completed);
        assert_eq!(positions(&root), (4, 4));
        assert!(recover_jobs(&root).unwrap().is_empty());
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), completed);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(verify_snapshot(&output).unwrap(), result);
    }

    /// A destination occupied after acceptance becomes a saved failure, not an overwrite.
    #[test]
    fn publication_failure_is_durable_and_preserves_the_occupied_destination() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let output = parent.path().join("snapshot");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &output).unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep.txt"), b"existing destination").unwrap();
        let failed = run_snapshot_job(&root, queued.job_id).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.revision.value(), 3);
        assert_eq!(failed.attempt_generation.value(), 2);
        assert!(failed.result.is_none());
        assert!(failed.error.as_ref().unwrap().contains("already exists"));
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), failed);
        assert_eq!(
            fs::read(output.join("keep.txt")).unwrap(),
            b"existing destination"
        );
        assert_eq!(fs::read_dir(output).unwrap().count(), 1);
        assert_eq!(positions(&root), (3, 3));
        assert!(run_snapshot_job(&root, queued.job_id).is_err());
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), failed);
        assert_eq!(positions(&root), (3, 3));
    }

    /// Return historical completion after an artifact moves without touching the
    /// replacement destination, old snapshot bytes, or committed job/change data.
    #[test]
    fn repeated_completion_does_not_recopy_or_revalidate_the_recorded_output() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let output = parent.path().join("snapshot");
        let moved = parent.path().join("moved-snapshot");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &output).unwrap();
        let completed = run_snapshot_job(&root, queued.job_id).unwrap();
        let recorded_result = completed.result.as_ref().unwrap();
        let snapshot_database = fs::read(output.join("metadata.sqlite")).unwrap();
        let snapshot_manifest = fs::read(output.join("snapshot.json")).unwrap();
        fs::rename(&output, &moved).unwrap();
        fs::create_dir(&output).unwrap();
        let sentinel = output.join("keep.txt");
        fs::write(&sentinel, b"new data at the old destination").unwrap();
        let before_positions = positions(&root);
        let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
        let metadata_bytes = fs::read(&database).unwrap();
        let marker_bytes = fs::read(root.join("format.json")).unwrap();

        let repeated = run_snapshot_job(&root, queued.job_id).unwrap();

        assert_eq!(repeated, completed);
        assert_eq!(positions(&root), before_positions);
        assert_eq!(fs::read(database).unwrap(), metadata_bytes);
        assert_eq!(fs::read(root.join("format.json")).unwrap(), marker_bytes);
        assert_eq!(
            fs::read(sentinel).unwrap(),
            b"new data at the old destination"
        );
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        assert_eq!(
            fs::read(moved.join("metadata.sqlite")).unwrap(),
            snapshot_database
        );
        assert_eq!(
            fs::read(moved.join("snapshot.json")).unwrap(),
            snapshot_manifest
        );
        let moved_report = verify_snapshot(&moved).unwrap();
        assert_eq!(moved_report.database_hash, recorded_result.database_hash);
        assert_eq!(moved_report.server_id, recorded_result.server_id);
        assert_ne!(moved_report.root, recorded_result.root);
        assert_eq!(recorded_result.root, output.canonicalize().unwrap());
    }

    /// Canceling a queued request is revision checked and never starts its output.
    #[test]
    fn queued_cancellation_rejects_stale_revisions_without_an_extra_change() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let output = parent.path().join("snapshot");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &output).unwrap();
        let wrong = PositiveCounter::from_u64(2).unwrap();
        assert!(matches!(
            cancel_job(&root, queued.job_id, wrong),
            Err(StorageError::JobTransition(
                JobTransitionError::RevisionMismatch { .. }
            ))
        ));
        assert_eq!(positions(&root), (1, 1));
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), queued);
        let canceled = cancel_job(&root, queued.job_id, queued.revision).unwrap();
        assert_eq!(canceled.state, JobState::Canceled);
        assert_eq!(canceled.revision.value(), 2);
        assert_eq!(canceled.attempt_generation, Counter::ZERO);
        assert!(run_snapshot_job(&root, queued.job_id).is_err());
        assert!(cancel_job(&root, queued.job_id, queued.revision).is_err());
        assert_eq!(positions(&root), (2, 2));
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), canceled);
        assert!(!output.exists());
    }

    /// Refuse unknown identities and bad list budgets instead of fabricating jobs.
    #[test]
    fn absent_jobs_and_invalid_limits_remain_explicit_errors() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let unknown = JobId::new();
        assert!(
            matches!(inspect_job(&root, unknown), Err(StorageError::JobNotFound(id)) if id == unknown)
        );
        assert!(
            matches!(run_snapshot_job(&root, unknown), Err(StorageError::JobNotFound(id)) if id == unknown)
        );
        let absent = parent.path().join("missing");
        for limit in [0, 101, u16::MAX] {
            assert!(matches!(
                list_jobs(&absent, limit),
                Err(StorageError::InvalidJob(_))
            ));
        }
        assert!(!absent.exists());
        assert_eq!(positions(&root), (0, 0));
    }

    /// Refuse old roots and unsafe destinations before accepting a queued request.
    #[test]
    fn old_metadata_and_unsafe_destinations_are_not_adopted() {
        let parent = tempfile::tempdir().unwrap();
        let old = parent.path().join("schema-one");
        super::support::create_schema1(&old);
        let old_two = parent.path().join("schema-two");
        super::support::create_schema2(&old_two);
        for (source, schema) in [(&old, 1), (&old_two, 2)] {
            let old_bytes = fs::read(source.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            assert!(matches!(
                enqueue_snapshot_job(source, &parent.path().join("old-output")),
                Err(StorageError::JobsUnavailable { found }) if found == schema
            ));
            assert_eq!(
                fs::read(source.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap(),
                old_bytes
            );
            assert_eq!(inspect(source).unwrap().schema_version, schema);
        }
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let occupied = parent.path().join("occupied");
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("keep.txt"), b"unchanged").unwrap();
        for output in [
            &occupied,
            &root.join("nested-snapshot"),
            &parent.path().join("missing-parent/out"),
        ] {
            assert!(enqueue_snapshot_job(&root, output).is_err());
        }
        assert_eq!(positions(&root), (0, 0));
        assert_eq!(fs::read(occupied.join("keep.txt")).unwrap(), b"unchanged");
    }

    /// A maintenance holder blocks job reads, writes, execution, and recovery.
    #[test]
    fn exclusive_maintenance_ownership_blocks_every_job_operation() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &parent.path().join("snapshot")).unwrap();
        let lock = fs::File::open(root.join(LOCK_FILE)).unwrap();
        FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(matches!(
            inspect_job(&root, queued.job_id),
            Err(StorageError::InUse)
        ));
        assert!(matches!(list_jobs(&root, 10), Err(StorageError::InUse)));
        assert!(matches!(
            jumpstart_storage::run_next_snapshot_job(&root),
            Err(StorageError::InUse)
        ));
        assert!(matches!(
            list_jobs_in_state(&root, JobState::Queued, 10),
            Err(StorageError::InUse)
        ));
        assert!(matches!(
            enqueue_snapshot_job(&root, &parent.path().join("another")),
            Err(StorageError::InUse)
        ));
        assert!(matches!(
            run_snapshot_job(&root, queued.job_id),
            Err(StorageError::InUse)
        ));
        assert!(matches!(
            cancel_job(&root, queued.job_id, queued.revision),
            Err(StorageError::InUse)
        ));
        assert!(matches!(recover_jobs(&root), Err(StorageError::InUse)));
        drop(lock);
        assert_eq!(inspect_job(&root, queued.job_id).unwrap(), queued);
    }

    /// Corrupt but valid JSON must not bypass full projection/history verification.
    #[test]
    fn inconsistent_projection_blocks_diagnostics_snapshots_and_further_writes() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &parent.path().join("snapshot")).unwrap();
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        let mut payload = serde_json::to_value(&queued).unwrap();
        payload["output_path"] = serde_json::Value::String(
            parent
                .path()
                .join("other-intent")
                .to_str()
                .unwrap()
                .to_owned(),
        );
        connection
            .execute(
                "UPDATE change_batches SET payload_json=?1 WHERE job_id=?2",
                params![
                    serde_json::to_string(&payload).unwrap(),
                    queued.job_id.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
        connection.close().unwrap();
        assert!(matches!(
            inspect_job(&root, queued.job_id),
            Err(StorageError::InvalidJob(_))
        ));
        assert!(matches!(inspect(&root), Err(StorageError::InvalidJob(_))));
        assert!(matches!(
            list_jobs_in_state(&root, JobState::Completed, 10),
            Err(StorageError::InvalidJob(_))
        ));
        assert!(enqueue_snapshot_job(&root, &parent.path().join("another")).is_err());
        assert!(
            jumpstart_storage::create_snapshot(&root, &parent.path().join("bad-copy")).is_err()
        );
        assert!(!parent.path().join("bad-copy").exists());
    }

    /// Even matching row/history JSON cannot substitute another source's result.
    #[test]
    fn completed_results_remain_bound_to_the_captured_source() {
        for changed in [
            "server_id",
            "sync_epoch",
            "schema_version",
            "scope",
            "change_seq",
        ] {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("metadata");
            let output = parent.path().join("snapshot");
            initialize(&root).unwrap();
            let queued = enqueue_snapshot_job(&root, &output).unwrap();
            let completed = run_snapshot_job(&root, queued.job_id).unwrap();
            let mut result = serde_json::to_value(completed.result.as_ref().unwrap()).unwrap();
            result[changed] = match changed {
                "server_id" => serde_json::to_value(jumpstart_domain::ServerId::new()).unwrap(),
                "sync_epoch" => serde_json::to_value(jumpstart_domain::SyncEpoch::new()).unwrap(),
                "schema_version" => serde_json::json!(2),
                "scope" => serde_json::json!("bootstrap_metadata"),
                "change_seq" => serde_json::json!("0"),
                _ => unreachable!(),
            };
            let mut projection = serde_json::to_value(&completed).unwrap();
            projection["result"] = result.clone();
            let connection =
                Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            connection
                .execute(
                    "UPDATE jobs SET result_json=?1 WHERE job_id=?2",
                    params![
                        serde_json::to_string(&result).unwrap(),
                        queued.job_id.as_uuid().as_bytes().as_slice()
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE change_batches SET payload_json=?1 WHERE job_id=?2 AND revision=4",
                    params![
                        serde_json::to_string(&projection).unwrap(),
                        queued.job_id.as_uuid().as_bytes().as_slice()
                    ],
                )
                .unwrap();
            connection.close().unwrap();
            assert!(
                matches!(
                    inspect_job(&root, queued.job_id),
                    Err(StorageError::InvalidJob(_))
                ),
                "{changed}"
            );
            assert!(
                matches!(inspect(&root), Err(StorageError::InvalidJob(_))),
                "{changed}"
            );
            assert!(
                verify_snapshot(&output).is_ok(),
                "the published artifact itself was not changed"
            );
        }
    }

    /// Exhausted change positions refuse a new job before a partial row can commit.
    #[test]
    fn exhausted_change_position_does_not_accept_partial_work() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        connection
            .execute(
                "UPDATE server_state SET change_seq=?1,change_floor=?1 WHERE singleton=1",
                [i64::MAX],
            )
            .unwrap();
        connection.close().unwrap();
        assert!(matches!(
            enqueue_snapshot_job(&root, &parent.path().join("snapshot")),
            Err(StorageError::ChangeSequenceExhausted)
        ));
        assert_eq!(list_jobs(&root, 100).unwrap(), Vec::new());
        assert_eq!(positions(&root), (i64::MAX as u64, 0));
    }

    /// Explicit job recovery cannot bypass a pending schema-upgrade workflow.
    #[test]
    fn migration_journal_and_sidecar_symlinks_block_recovery() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        fs::write(root.join(MIGRATION_FILE), b"retained migration evidence").unwrap();
        assert!(matches!(
            recover_jobs(&root),
            Err(StorageError::NeedsRecovery(_))
        ));
        assert_eq!(
            fs::read(root.join(MIGRATION_FILE)).unwrap(),
            b"retained migration evidence"
        );
        fs::remove_file(root.join(MIGRATION_FILE)).unwrap();
        let target = parent.path().join("unrelated");
        fs::write(&target, b"do not change").unwrap();
        let sidecar = root
            .join(DATABASE_DIRECTORY)
            .join(format!("{DATABASE_FILE}-wal"));
        std::os::unix::fs::symlink(&target, &sidecar).unwrap();
        assert!(recover_jobs(&root).is_err());
        assert_eq!(fs::read(target).unwrap(), b"do not change");
        assert!(
            fs::symlink_metadata(sidecar)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

/// Refuse job APIs on unsupported platforms without creating paths or metadata.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_job_operations_do_not_touch_the_filesystem() {
    use jumpstart_domain::{JobId, JobState, PositiveCounter};
    use jumpstart_storage::{
        StorageError, cancel_job, enqueue_snapshot_job, inspect_job, list_jobs, list_jobs_in_state,
        recover_jobs, run_snapshot_job,
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("metadata");
    let output = temporary.path().join("snapshot");
    let id = JobId::new();
    assert!(matches!(
        enqueue_snapshot_job(&root, &output),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        run_snapshot_job(&root, id),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        inspect_job(&root, id),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        cancel_job(&root, id, PositiveCounter::ONE),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        list_jobs(&root, 10),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        list_jobs_in_state(&root, JobState::Queued, 10),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        recover_jobs(&root),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 0);
}
