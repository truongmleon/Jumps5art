//! Checks bounded job-change pages without confusing past projections with current status.
//!
//! For example, an app that last saw position 2 must receive the later Running,
//! Verifying, and Completed records in order, even if that job is already finished.
//! These tests also keep an unavailable old history distinct from an empty page
//! and prove that reading changes never performs recovery or starts queued work.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use fs2::FileExt;
    use jumpstart_domain::{Counter, JobState, SyncEpoch};
    use jumpstart_storage::{
        DATABASE_DIRECTORY, DATABASE_FILE, LOCK_FILE, MAX_JOB_CHANGE_PAGE_BYTES,
        MAX_JOB_CHANGE_PAGE_RECORDS, MIGRATION_FILE, StorageError, cancel_job,
        enqueue_snapshot_job, initialize, inspect, inspect_job, job_history, migrate,
        pull_job_changes, run_snapshot_job,
    };
    use rusqlite::{Connection, params};

    /// Convert a test's known nonnegative position into the checked counter type.
    ///
    /// `value` is a valid `u64` position. Returns `Counter`; panics if a fixture
    /// accidentally supplies an unsupported value rather than silently narrowing it.
    fn position(value: u64) -> Counter {
        Counter::from_u64(value).unwrap()
    }

    /// Capture directory entries, link destinations, and exact regular-file bytes.
    ///
    /// `root` names a scratch tree. Returns an ordered map keyed by relative paths;
    /// each value starts with a kind tag so empty files and directories differ.
    /// Panics on fixture I/O errors or unexpected special files; never follows links.
    fn fingerprint(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut state = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let bytes = if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                b"directory".to_vec()
            } else if metadata.file_type().is_symlink() {
                format!("link:{}", fs::read_link(&path).unwrap().display()).into_bytes()
            } else {
                assert!(metadata.is_file());
                let mut bytes = b"file:".to_vec();
                bytes.extend(fs::read(&path).unwrap());
                bytes
            };
            state.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
        }
        state
    }

    /// Page through interleaved real jobs using the original saved projections,
    /// preserving global order, whole records, and all source/output bytes.
    #[test]
    fn paginated_changes_preserve_original_projections_and_global_order() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let metadata = initialize(&root).unwrap();
        let a_output = parent.path().join("successful-copy");
        let b_output = parent.path().join("canceled-copy");
        let c_output = parent.path().join("occupied-copy");
        let a = enqueue_snapshot_job(&root, &a_output).unwrap();
        let b = enqueue_snapshot_job(&root, &b_output).unwrap();
        let completed = run_snapshot_job(&root, a.job_id).unwrap();
        assert_eq!(completed.state, JobState::Completed);
        let canceled = cancel_job(&root, b.job_id, b.revision).unwrap();
        assert_eq!(canceled.state, JobState::Canceled);
        let c = enqueue_snapshot_job(&root, &c_output).unwrap();
        fs::create_dir(&c_output).unwrap();
        fs::write(c_output.join("keep.txt"), b"keep occupied output").unwrap();
        let failed = run_snapshot_job(&root, c.job_id).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        let a_history = job_history(&root, a.job_id).unwrap();
        let b_history = job_history(&root, b.job_id).unwrap();
        let c_history = job_history(&root, c.job_id).unwrap();
        let expected = vec![
            (1_u64, a_history[0].clone()),
            (2, b_history[0].clone()),
            (3, a_history[1].clone()),
            (4, a_history[2].clone()),
            (5, a_history[3].clone()),
            (6, b_history[1].clone()),
            (7, c_history[0].clone()),
            (8, c_history[1].clone()),
            (9, c_history[2].clone()),
        ];
        let source_before = fingerprint(&root);
        let copy_before = fingerprint(&a_output);
        let occupied_before = fingerprint(&c_output);
        let mut cursor = Counter::ZERO;
        let mut observed = Vec::new();
        loop {
            let page = pull_job_changes(&root, metadata.sync_epoch, cursor, 2).unwrap();
            assert_eq!(page.server_id, metadata.server_id);
            assert_eq!(page.sync_epoch, metadata.sync_epoch);
            assert_eq!(page.after, cursor);
            assert_eq!(page.minimum, Counter::ZERO);
            assert_eq!(page.high_water, position(9));
            assert!(page.batches.len() <= 2);
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_JOB_CHANGE_PAGE_BYTES);
            if page.batches.is_empty() {
                assert_eq!(page.next, cursor);
                break;
            }
            let next = page.next;
            assert_eq!(next.value(), page.batches.last().unwrap().seq.value());
            assert!(next > cursor && next <= page.high_water);
            for batch in page.batches {
                assert_eq!(batch.payload_version, 1);
                observed.push((batch.seq.value(), batch.job));
            }
            cursor = next;
        }
        assert_eq!(observed, expected);
        assert_eq!(observed[0].1, a);
        assert_ne!(observed[0].1, completed);
        assert!(observed[0].1.result.is_none());
        assert_eq!(inspect_job(&root, completed.job_id).unwrap(), completed);
        assert_eq!(fingerprint(&root), source_before);
        assert_eq!(fingerprint(&a_output), copy_before);
        assert_eq!(fingerprint(&c_output), occupied_before);
        assert!(!b_output.exists());
    }

    /// Empty current pages retain the caller's exact cursor and captured identity.
    #[test]
    fn fresh_and_at_head_pages_are_explicitly_empty_without_advancing() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let metadata = initialize(&root).unwrap();
        let before = fingerprint(&root);
        let fresh = pull_job_changes(
            &root,
            metadata.sync_epoch,
            Counter::ZERO,
            MAX_JOB_CHANGE_PAGE_RECORDS,
        )
        .unwrap();
        assert!(fresh.batches.is_empty());
        assert_eq!(fresh.server_id, metadata.server_id);
        assert_eq!(fresh.sync_epoch, metadata.sync_epoch);
        assert_eq!(fresh.after, Counter::ZERO);
        assert_eq!(fresh.next, Counter::ZERO);
        assert_eq!(fresh.high_water, Counter::ZERO);
        assert_eq!(fresh.minimum, Counter::ZERO);
        assert_eq!(fingerprint(&root), before);
        let output = parent.path().join("unstarted-copy");
        let queued = enqueue_snapshot_job(&root, &output).unwrap();
        let at_head = position(queued.created_seq.value());
        let before = fingerprint(&root);
        let empty = pull_job_changes(&root, metadata.sync_epoch, at_head, 1).unwrap();
        assert!(empty.batches.is_empty());
        assert_eq!(empty.after, at_head);
        assert_eq!(empty.next, at_head);
        assert_eq!(empty.high_water, at_head);
        assert_eq!(empty.minimum, Counter::ZERO);
        assert_eq!(fingerprint(&root), before);
        assert!(!output.exists());
    }

    /// Invalid budgets fail before I/O; wrong histories and future positions remain
    /// explicit errors rather than successful empty responses.
    #[test]
    fn invalid_limits_epochs_and_future_cursors_are_refused_without_mutation() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        let epoch = SyncEpoch::new();
        for limit in [0, MAX_JOB_CHANGE_PAGE_RECORDS + 1, u16::MAX] {
            assert!(matches!(
                pull_job_changes(&missing, epoch, Counter::ZERO, limit),
                Err(StorageError::InvalidJobChangeCursor(_))
            ));
        }
        assert!(!missing.exists());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        let root = parent.path().join("metadata");
        let metadata = initialize(&root).unwrap();
        let wrong = SyncEpoch::new();
        assert_ne!(wrong, metadata.sync_epoch);
        let before = fingerprint(&root);
        assert!(matches!(
            pull_job_changes(&root, wrong, Counter::ZERO, 1),
            Err(StorageError::JobChangeEpochMismatch { requested, current })
                if requested == wrong && current == metadata.sync_epoch
        ));
        for after in [position(1), Counter::MAX] {
            assert!(matches!(
                pull_job_changes(&root, metadata.sync_epoch, after, 1),
                Err(StorageError::InvalidJobChangeCursor(_))
            ));
        }
        assert_eq!(fingerprint(&root), before);
    }

    /// A migrated root cannot invent history below its preserved floor, while a
    /// cursor at that floor can read new changes and preserves their real numbers.
    #[test]
    fn migrated_floor_requires_resync_below_the_inclusive_minimum() {
        for baseline in [42_u64, Counter::MAX.value()] {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("schema-two");
            let original = super::support::create_schema2(&root);
            let connection =
                Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
            connection
                .execute(
                    "UPDATE server_state SET change_seq=?1",
                    [i64::try_from(baseline).unwrap()],
                )
                .unwrap();
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            connection.close().unwrap();
            let metadata = migrate(&root, &parent.path().join("before-three"))
                .unwrap()
                .metadata;
            assert_eq!(metadata.sync_epoch, original.sync_epoch);
            let before = fingerprint(&root);
            assert!(matches!(
                pull_job_changes(&root, metadata.sync_epoch, position(baseline - 1), 1),
                Err(StorageError::JobChangesUnavailable { after, minimum })
                    if after == position(baseline - 1) && minimum == position(baseline)
            ));
            let empty =
                pull_job_changes(&root, metadata.sync_epoch, position(baseline), 1).unwrap();
            assert!(empty.batches.is_empty());
            assert_eq!(empty.after, position(baseline));
            assert_eq!(empty.next, position(baseline));
            assert_eq!(empty.minimum, position(baseline));
            assert_eq!(empty.high_water, position(baseline));
            assert_eq!(fingerprint(&root), before);
            if baseline == Counter::MAX.value() {
                // A valid exhausted position still permits an empty read page;
                // computing an unnecessary "next plus one" must not overflow.
                continue;
            }
            let output = parent.path().join("new-copy");
            let queued = enqueue_snapshot_job(&root, &output).unwrap();
            assert_eq!(queued.created_seq.value(), baseline + 1);
            let page = pull_job_changes(&root, metadata.sync_epoch, position(baseline), 1).unwrap();
            assert_eq!(page.batches.len(), 1);
            assert_eq!(page.batches[0].seq, queued.created_seq);
            assert_eq!(page.batches[0].job, queued);
            assert_eq!(page.next, position(baseline + 1));
            assert_eq!(page.high_water, position(baseline + 1));
            assert_eq!(page.minimum, position(baseline));
            assert!(!output.exists());
        }
    }

    /// Missing or older storage must not be initialized or presented as an empty feed.
    #[test]
    fn missing_and_old_roots_are_refused_without_adoption() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        assert!(pull_job_changes(&missing, SyncEpoch::new(), Counter::ZERO, 1).is_err());
        assert!(!missing.exists());
        let one = parent.path().join("one");
        let two = parent.path().join("two");
        let one_report = super::support::create_schema1(&one);
        let two_report = super::support::create_schema2(&two);
        for (root, metadata, schema) in [(&one, one_report, 1), (&two, two_report, 2)] {
            let before = fingerprint(root);
            assert!(matches!(
                pull_job_changes(root, metadata.sync_epoch, Counter::ZERO, 1),
                Err(StorageError::JobsUnavailable { found }) if found == schema
            ));
            assert_eq!(fingerprint(root), before);
            assert_eq!(inspect(root).unwrap().schema_version, schema);
        }
    }

    /// Reading changes cannot bypass a busy owner or implicitly repair retained
    /// migration evidence or a committed WAL; those need explicit maintenance.
    #[test]
    fn busy_and_pending_recovery_states_are_refused_without_implicit_repair() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let metadata = initialize(&root).unwrap();
        let lock = fs::File::open(root.join(LOCK_FILE)).unwrap();
        FileExt::try_lock_exclusive(&lock).unwrap();
        let before = fingerprint(&root);
        assert!(matches!(
            pull_job_changes(&root, metadata.sync_epoch, Counter::ZERO, 1),
            Err(StorageError::InUse)
        ));
        assert_eq!(fingerprint(&root), before);
        drop(lock);
        fs::write(root.join(MIGRATION_FILE), b"retain migration evidence").unwrap();
        let before = fingerprint(&root);
        assert!(matches!(
            pull_job_changes(&root, metadata.sync_epoch, Counter::ZERO, 1),
            Err(StorageError::NeedsRecovery(_))
        ));
        assert_eq!(fingerprint(&root), before);
        fs::remove_file(root.join(MIGRATION_FILE)).unwrap();
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        connection.execute_batch(
            "PRAGMA wal_autocheckpoint=0; UPDATE schema_migrations SET applied_at=applied_at || '-wal' WHERE version=3;",
        ).unwrap();
        let before = fingerprint(&root);
        assert!(matches!(
            pull_job_changes(&root, metadata.sync_epoch, Counter::ZERO, 1),
            Err(StorageError::NeedsRecovery(_))
        ));
        assert_eq!(fingerprint(&root), before);
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection.close().unwrap();
    }

    /// Even an at-head request must reject a valid-JSON projection that disagrees
    /// with its durable job row, rather than claiming a healthy empty page.
    #[test]
    fn semantic_corruption_is_refused_even_when_no_new_batches_are_requested() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("metadata");
        let metadata = initialize(&root).unwrap();
        let queued = enqueue_snapshot_job(&root, &parent.path().join("snapshot")).unwrap();
        let mut projection = serde_json::to_value(&queued).unwrap();
        projection["output_path"] = serde_json::json!(parent.path().join("different-intent"));
        let connection =
            Connection::open(root.join(DATABASE_DIRECTORY).join(DATABASE_FILE)).unwrap();
        connection
            .execute(
                "UPDATE change_batches SET payload_json=?1 WHERE job_id=?2 AND revision=1",
                params![
                    serde_json::to_string(&projection).unwrap(),
                    queued.job_id.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection.close().unwrap();
        let before = fingerprint(&root);
        assert!(matches!(
            pull_job_changes(&root, metadata.sync_epoch, position(1), 1),
            Err(StorageError::InvalidJob(_))
        ));
        assert_eq!(fingerprint(&root), before);
    }
}

/// Unsupported platforms report the boundary without creating any requested path.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_changes_do_not_touch_the_filesystem() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("missing");
    assert!(matches!(
        jumpstart_storage::pull_job_changes(
            &root,
            jumpstart_domain::SyncEpoch::new(),
            jumpstart_domain::Counter::ZERO,
            1,
        ),
        Err(jumpstart_storage::StorageError::UnsupportedPlatform)
    ));
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}
