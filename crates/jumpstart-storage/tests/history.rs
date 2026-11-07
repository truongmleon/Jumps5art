//! Checks that job history explains earlier decisions without changing or rerunning work.
//!
//! A completed request should still show its original Queued, Running, and
//! Verifying projections. Canceled and failed requests keep their actual reasons.
//! Reads must retain existing metadata, output, and unrelated files unchanged.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::fs;

    use fs2::FileExt;
    use jumpstart_domain::{JobId, JobState};
    use jumpstart_storage::{
        DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, LOCK_FILE, StorageError, cancel_job,
        enqueue_snapshot_job, initialize, inspect_job, job_history, run_snapshot_job,
        verify_snapshot,
    };
    use rusqlite::{Connection, params};

    /// Return historical values in revision order while preserving all inspected files.
    #[test]
    fn completed_history_preserves_earlier_values_without_execution_or_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("metadata");
        let output = temporary.path().join("first-snapshot");
        let unstarted_output = temporary.path().join("later-snapshot");
        initialize(&root).unwrap();
        let accepted = enqueue_snapshot_job(&root, &output).unwrap();
        let unstarted = enqueue_snapshot_job(&root, &unstarted_output).unwrap();
        let completed = run_snapshot_job(&root, accepted.job_id).unwrap();
        let sentinel = root.join("secrets/keep.txt");
        fs::write(&sentinel, b"unrelated retained contents").unwrap();
        let preserved = [
            root.join(DATABASE_DIRECTORY).join(DATABASE_FILE),
            root.join(FORMAT_FILE),
            sentinel,
            output.join("snapshot.json"),
            output.join("metadata.sqlite"),
        ];
        let contents_before: Vec<_> = preserved
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let modified_before: Vec<_> = preserved
            .iter()
            .map(|path| fs::metadata(path).unwrap().modified().unwrap())
            .collect();
        let root_entries_before = fs::read_dir(&root).unwrap().count();

        let history = job_history(&root, accepted.job_id).unwrap();

        assert_eq!(history.len(), 4);
        assert_eq!(history[0], accepted);
        assert_eq!(history[3], completed);
        assert_eq!(
            history.iter().map(|job| job.state).collect::<Vec<_>>(),
            vec![
                JobState::Queued,
                JobState::Running,
                JobState::Verifying,
                JobState::Completed
            ]
        );
        assert_eq!(
            history
                .iter()
                .map(|job| job.revision.value())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            history
                .iter()
                .map(|job| job.attempt_generation.value())
                .collect::<Vec<_>>(),
            vec![0, 1, 1, 2]
        );
        assert!(
            history[..3]
                .iter()
                .all(|job| job.result.is_none() && job.error.is_none())
        );
        let result = history[3].result.as_ref().unwrap();
        assert_eq!(result.change_seq.unwrap().value(), 3);
        assert_eq!(&verify_snapshot(&output).unwrap(), result);
        assert_eq!(
            job_history(&root, unstarted.job_id).unwrap(),
            vec![unstarted.clone()]
        );
        assert_eq!(inspect_job(&root, unstarted.job_id).unwrap(), unstarted);
        assert!(!unstarted_output.exists());
        assert_eq!(
            preserved
                .iter()
                .map(|path| fs::read(path).unwrap())
                .collect::<Vec<_>>(),
            contents_before
        );
        assert_eq!(
            preserved
                .iter()
                .map(|path| fs::metadata(path).unwrap().modified().unwrap())
                .collect::<Vec<_>>(),
            modified_before
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), root_entries_before);
    }

    /// Keep canceled and failed histories distinct without inventing successful output.
    #[test]
    fn canceled_and_failed_histories_keep_their_recorded_outcomes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("metadata");
        initialize(&root).unwrap();
        let canceled_output = temporary.path().join("canceled-snapshot");
        let queued = enqueue_snapshot_job(&root, &canceled_output).unwrap();
        let canceled = cancel_job(&root, queued.job_id, queued.revision).unwrap();
        let canceled_history = job_history(&root, queued.job_id).unwrap();
        assert_eq!(canceled_history, vec![queued, canceled]);
        assert!(!canceled_output.exists());

        let occupied = temporary.path().join("occupied-snapshot");
        let queued = enqueue_snapshot_job(&root, &occupied).unwrap();
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("keep.txt"), b"existing destination").unwrap();
        let failed = run_snapshot_job(&root, queued.job_id).unwrap();
        let history = job_history(&root, queued.job_id).unwrap();
        assert_eq!(
            history.iter().map(|job| job.state).collect::<Vec<_>>(),
            vec![JobState::Queued, JobState::Running, JobState::Failed]
        );
        assert_eq!(history[0], queued);
        assert_eq!(history[2], failed);
        assert!(history[0].error.is_none() && history[1].error.is_none());
        assert!(
            history[2]
                .error
                .as_ref()
                .unwrap()
                .contains("already exists")
        );
        assert!(history.iter().all(|job| job.result.is_none()));
        assert_eq!(
            fs::read(occupied.join("keep.txt")).unwrap(),
            b"existing destination"
        );
        assert_eq!(fs::read_dir(occupied).unwrap().count(), 1);
    }

    /// Refuse missing identities, unsupported roots, and an active maintenance owner.
    #[test]
    fn history_requires_known_supported_available_metadata_without_initialization() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("metadata");
        initialize(&root).unwrap();
        let unknown = JobId::new();
        assert!(
            matches!(job_history(&root, unknown), Err(StorageError::JobNotFound(id)) if id == unknown)
        );
        let missing = temporary.path().join("missing");
        assert!(job_history(&missing, unknown).is_err());
        assert!(!missing.exists());
        let old_one = temporary.path().join("schema-one");
        let old_two = temporary.path().join("schema-two");
        super::support::create_schema1(&old_one);
        super::support::create_schema2(&old_two);
        for (old, version) in [(&old_one, 1), (&old_two, 2)] {
            let database = old.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
            let before = fs::read(&database).unwrap();
            assert!(
                matches!(job_history(old, unknown), Err(StorageError::JobsUnavailable { found }) if found == version)
            );
            assert_eq!(fs::read(database).unwrap(), before);
        }
        let queued = enqueue_snapshot_job(&root, &temporary.path().join("snapshot")).unwrap();
        let lock = fs::File::open(root.join(LOCK_FILE)).unwrap();
        FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(matches!(
            job_history(&root, queued.job_id),
            Err(StorageError::InUse)
        ));
        drop(lock);
        assert_eq!(job_history(&root, queued.job_id).unwrap(), vec![queued]);
    }

    /// Reject semantically invalid projection JSON before returning an invented history.
    #[test]
    fn semantic_projection_corruption_is_refused_without_repair() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("metadata");
        initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &temporary.path().join("snapshot")).unwrap();
        let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
        let connection = Connection::open(&database).unwrap();
        let mut projection = serde_json::to_value(&queued).unwrap();
        projection["revision"] = serde_json::json!("2");
        connection
            .execute(
                "UPDATE change_batches SET payload_json=?1 WHERE job_id=?2",
                params![
                    serde_json::to_string(&projection).unwrap(),
                    queued.job_id.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
        connection.close().unwrap();
        let before = fs::read(&database).unwrap();
        assert!(matches!(
            job_history(&root, queued.job_id),
            Err(StorageError::InvalidJob(_))
        ));
        assert_eq!(fs::read(database).unwrap(), before);
    }
}

/// Refuse unsupported platforms without creating the requested metadata directory.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_history_reads_leave_paths_absent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("metadata");
    assert!(matches!(
        jumpstart_storage::job_history(&root, jumpstart_domain::JobId::new()),
        Err(jumpstart_storage::StorageError::UnsupportedPlatform)
    ));
    assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 0);
}
