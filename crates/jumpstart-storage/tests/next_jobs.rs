//! Checks bounded queue execution without choosing a request ID first.
//!
//! Acceptance order decides which request runs. One call attempts at most one
//! request, even when that attempt fails, and competing runners cannot execute
//! the same request twice. Empty or unavailable storage must remain explicit.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::fs;
    use std::sync::{Arc, Barrier};

    use jumpstart_domain::JobState;
    use jumpstart_storage::{
        DATABASE_DIRECTORY, DATABASE_FILE, FORMAT_FILE, StorageError, cancel_job,
        enqueue_snapshot_job, initialize, inspect, inspect_job, job_history, run_next_snapshot_job,
        run_snapshot_job, verify_snapshot,
    };
    use rusqlite::{Connection, params};

    /// Select the oldest queued acceptance and preserve terminal/empty results.
    #[test]
    fn next_runs_oldest_queued_once_and_empty_queue_does_not_change_metadata() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let canceled = enqueue_snapshot_job(&root, &parent.path().join("canceled")).unwrap();
        let canceled = cancel_job(&root, canceled.job_id, canceled.revision).unwrap();
        let done = enqueue_snapshot_job(&root, &parent.path().join("already-done")).unwrap();
        let done = run_snapshot_job(&root, done.job_id).unwrap();
        let first_output = parent.path().join("first");
        let first = enqueue_snapshot_job(&root, &first_output).unwrap();
        let second_output = parent.path().join("second");
        let second = enqueue_snapshot_job(&root, &second_output).unwrap();

        let first_done = run_next_snapshot_job(&root).unwrap().unwrap();
        assert_eq!(first_done.job_id, first.job_id);
        assert_eq!(first_done.state, JobState::Completed);
        assert_eq!(first_done.revision.value(), 4);
        assert_eq!(inspect_job(&root, second.job_id).unwrap(), second);
        assert!(!second_output.exists());
        assert_eq!(
            verify_snapshot(&first_output).unwrap(),
            first_done.result.unwrap()
        );
        let second_done = run_next_snapshot_job(&root).unwrap().unwrap();
        assert_eq!(second_done.job_id, second.job_id);
        assert_eq!(second_done.state, JobState::Completed);
        assert_eq!(job_history(&root, second.job_id).unwrap().len(), 4);
        assert_eq!(inspect_job(&root, canceled.job_id).unwrap(), canceled);
        assert_eq!(inspect_job(&root, done.job_id).unwrap(), done);

        let before = inspect(&root).unwrap();
        let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
        let database_bytes = fs::read(&database).unwrap();
        let marker = fs::read(root.join(FORMAT_FILE)).unwrap();
        let entries = fs::read_dir(root.join(DATABASE_DIRECTORY)).unwrap().count();
        assert!(run_next_snapshot_job(&root).unwrap().is_none());
        assert!(run_next_snapshot_job(&root).unwrap().is_none());
        assert_eq!(inspect(&root).unwrap(), before);
        assert_eq!(fs::read(database).unwrap(), database_bytes);
        assert_eq!(fs::read(root.join(FORMAT_FILE)).unwrap(), marker);
        assert_eq!(
            fs::read_dir(root.join(DATABASE_DIRECTORY)).unwrap().count(),
            entries
        );
        assert!(!parent.path().join("canceled").exists());
    }

    /// A failed first request is returned without draining or retrying the queue.
    #[test]
    fn failure_is_saved_and_later_queued_work_waits_for_another_call() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let occupied = parent.path().join("occupied");
        let first = enqueue_snapshot_job(&root, &occupied).unwrap();
        let later_output = parent.path().join("later");
        let later = enqueue_snapshot_job(&root, &later_output).unwrap();
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("keep.txt"), b"occupied destination").unwrap();

        let failed = run_next_snapshot_job(&root).unwrap().unwrap();
        assert_eq!(failed.job_id, first.job_id);
        assert_eq!(failed.state, JobState::Failed);
        assert!(failed.error.is_some());
        assert_eq!(job_history(&root, first.job_id).unwrap().len(), 3);
        assert_eq!(inspect_job(&root, later.job_id).unwrap(), later);
        assert!(!later_output.exists());
        assert_eq!(
            fs::read(occupied.join("keep.txt")).unwrap(),
            b"occupied destination"
        );
        let completed = run_next_snapshot_job(&root).unwrap().unwrap();
        assert_eq!(completed.job_id, later.job_id);
        assert_eq!(completed.state, JobState::Completed);
        assert_eq!(inspect_job(&root, first.job_id).unwrap(), failed);
        assert!(run_next_snapshot_job(&root).unwrap().is_none());
    }

    /// Two simultaneous callers either contend or observe the completed queue;
    /// exactly one executes its only request and publishes one verified snapshot.
    #[test]
    fn competing_runners_execute_one_request_only_once() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let output = parent.path().join("snapshot");
        let accepted = enqueue_snapshot_job(&root, &output).unwrap();
        let start = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let root = root.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    run_next_snapshot_job(&root)
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            outcomes.iter().filter(|v| matches!(v, Ok(Some(_)))).count(),
            1
        );
        for outcome in outcomes {
            match outcome {
                Ok(Some(job)) => {
                    assert_eq!(job.job_id, accepted.job_id);
                    assert_eq!(job.state, JobState::Completed);
                }
                Ok(None) | Err(StorageError::InUse) => {}
                other => panic!("unexpected competing-runner outcome: {other:?}"),
            }
        }
        assert_eq!(inspect(&root).unwrap().change_seq.unwrap().value(), 4);
        assert_eq!(job_history(&root, accepted.job_id).unwrap().len(), 4);
        verify_snapshot(&output).unwrap();
    }

    /// Missing/old roots and inconsistent history are errors rather than empty
    /// work, and none may be initialized, upgraded, or silently repaired.
    #[test]
    fn unavailable_or_corrupt_metadata_is_not_treated_as_an_empty_queue() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        assert!(run_next_snapshot_job(&missing).is_err());
        assert!(!missing.exists());
        for schema in [1, 2] {
            let root = parent.path().join(format!("schema-{schema}"));
            if schema == 1 {
                super::support::create_schema1(&root);
            } else {
                super::support::create_schema2(&root);
            }
            let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
            let before = fs::read(&database).unwrap();
            assert!(matches!(run_next_snapshot_job(&root),
                Err(StorageError::JobsUnavailable { found }) if found == schema));
            assert_eq!(fs::read(database).unwrap(), before);
            assert_eq!(inspect(&root).unwrap().schema_version, schema);
        }
        let root = parent.path().join("metadata");
        initialize(&root).unwrap();
        let accepted = enqueue_snapshot_job(&root, &parent.path().join("never-run")).unwrap();
        let mut payload = serde_json::to_value(&accepted).unwrap();
        payload["output_path"] = serde_json::json!(parent.path().join("different"));
        let database = root.join(DATABASE_DIRECTORY).join(DATABASE_FILE);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE change_batches SET payload_json=?1",
                params![serde_json::to_string(&payload).unwrap()],
            )
            .unwrap();
        connection.close().unwrap();
        let before = fs::read(&database).unwrap();
        assert!(matches!(
            run_next_snapshot_job(&root),
            Err(StorageError::InvalidJob(_))
        ));
        assert_eq!(fs::read(database).unwrap(), before);
        assert!(!parent.path().join("never-run").exists());
    }
}

/// An unsupported runner refuses the operation without creating a requested path.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_next_runner_does_not_create_metadata() {
    let parent = tempfile::tempdir().unwrap();
    assert!(matches!(
        jumpstart_storage::run_next_snapshot_job(&parent.path().join("missing")),
        Err(jumpstart_storage::StorageError::UnsupportedPlatform)
    ));
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}
