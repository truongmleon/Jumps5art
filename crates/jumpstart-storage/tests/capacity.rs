//! Checks Jumpstart's free-space warnings without filling a real disk.
//!
//! For example, a download should not count the recovery reserve as ordinary
//! available space. Fixed byte counts test that calculation at its boundaries;
//! temporary directories test that reading real capacity leaves existing files
//! intact. These tests establish capacity behavior, not disk health or safe writes.

use jumpstart_storage::{StorageError, assess_capacity};

const GIBIBYTE: u64 = 1024 * 1024 * 1024;

/// Exercise warning and reserve boundaries without depending on host free space.
#[test]
fn default_thresholds_have_exact_boundary_comparisons() {
    let total = 100 * GIBIBYTE;
    let warning = 10 * GIBIBYTE;
    let reserve = 256 * 1024 * 1024;
    for (available, warns, reserved) in [
        (warning + 1, false, false),
        (warning, false, false),
        (warning - 1, true, false),
        (reserve + 1, true, false),
        (reserve, true, true),
        (reserve - 1, true, true),
        (0, true, true),
    ] {
        let assessment = assess_capacity(total, available).unwrap();
        assert_eq!(assessment.warning_below_bytes.value(), warning);
        assert_eq!(assessment.safety_reserve_bytes.value(), reserve);
        assert_eq!(assessment.below_warning_threshold, warns);
        assert_eq!(assessment.at_or_below_safety_reserve, reserved);
        assert_eq!(
            assessment.available_above_reserve_bytes.value(),
            available.saturating_sub(reserve)
        );
    }
}

/// Test whole-byte rounding for tiny totals and around the documented size limits.
#[test]
fn rounding_and_large_volume_defaults_are_exact() {
    for (total, warning, reserve) in [
        (1, 1, 0),
        (9, 1, 0),
        (10, 1, 0),
        (11, 2, 0),
        (99, 10, 0),
        (100, 10, 1),
        (101, 11, 1),
        (50 * GIBIBYTE - 1, 5 * GIBIBYTE, 256 * 1024 * 1024),
        (50 * GIBIBYTE, 5 * GIBIBYTE, 256 * 1024 * 1024),
        (50 * GIBIBYTE + 1, 5 * GIBIBYTE + 1, 256 * 1024 * 1024),
    ] {
        let assessment = assess_capacity(total, total).unwrap();
        assert_eq!(assessment.warning_below_bytes.value(), warning);
        assert_eq!(assessment.safety_reserve_bytes.value(), reserve);
        assert!(!assessment.below_warning_threshold);
        assert!(!assessment.at_or_below_safety_reserve);
    }
    let reserve_limit = 256 * 1024 * 1024;
    for total in [
        reserve_limit * 100 - 1,
        reserve_limit * 100,
        reserve_limit * 100 + 1,
    ] {
        assert_eq!(
            assess_capacity(total, total)
                .unwrap()
                .safety_reserve_bytes
                .value(),
            (total / 100).min(reserve_limit)
        );
    }
}

/// Ensure maximum supported counts do not overflow percentage or reserve math.
#[test]
fn maximum_counter_capacity_is_assessed_without_overflow() {
    let maximum = i64::MAX as u64;
    let assessment = assess_capacity(maximum, maximum).unwrap();
    assert_eq!(assessment.warning_below_bytes.value(), maximum / 10 + 1);
    assert_eq!(assessment.safety_reserve_bytes.value(), 256 * 1024 * 1024);
    assert_eq!(
        assessment.available_above_reserve_bytes.value(),
        maximum - 256 * 1024 * 1024
    );
}

/// Reject impossible inputs rather than turning them into reassuring assessments.
#[test]
fn invalid_assessment_inputs_are_rejected() {
    for (total, available) in [
        (0, 0),
        (0, 1),
        (10, 11),
        (i64::MAX as u64 + 1, 0),
        (1, i64::MAX as u64 + 1),
        (u64::MAX, u64::MAX),
    ] {
        assert!(matches!(
            assess_capacity(total, available),
            Err(StorageError::InvalidCapacity(_))
        ));
    }
}

/// Keep byte counts exact decimal strings and threshold results ordinary booleans.
#[test]
fn assessment_json_preserves_exact_byte_counts() {
    let value = serde_json::to_value(assess_capacity(1001, 100).unwrap()).unwrap();
    assert_eq!(value["warning_below_bytes"], "101");
    assert_eq!(value["safety_reserve_bytes"], "10");
    assert_eq!(value["available_above_reserve_bytes"], "90");
    assert_eq!(value["below_warning_threshold"], true);
    assert_eq!(value["at_or_below_safety_reserve"], false);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod filesystem {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use jumpstart_storage::{CapacityScope, StorageError, assess_capacity, inspect_capacity};

    /// Record names, contents, permissions, and modification times in a small tree.
    ///
    /// # Arguments
    ///
    /// * `root` — `&Path` of the directory containing only test-created entries.
    ///
    /// # Returns
    ///
    /// A `BTreeMap` from relative paths to content, permission bits, and mtime.
    /// Directories use `None` for content; regular files use `Some(Vec<u8>)`.
    fn fingerprint(root: &Path) -> BTreeMap<PathBuf, (Option<Vec<u8>>, u32, SystemTime)> {
        let mut result = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let contents = if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                None
            } else {
                Some(fs::read(&path).unwrap())
            };
            result.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                (
                    contents,
                    metadata.permissions().mode(),
                    metadata.modified().unwrap(),
                ),
            );
        }
        result
    }

    /// Inspect actual space without creating bookkeeping or changing existing files.
    #[test]
    fn inspection_reports_one_consistent_observation_and_preserves_entries() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("ordinary-folder");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("photo.jpg"), b"existing source bytes").unwrap();
        fs::write(root.join("nested/notes.txt"), b"existing notes").unwrap();
        fs::set_permissions(root.join("photo.jpg"), fs::Permissions::from_mode(0o640)).unwrap();
        let before = fingerprint(&root);

        let report = inspect_capacity(&root).unwrap();

        assert_eq!(fingerprint(&root), before);
        assert_eq!(report.path, root.canonicalize().unwrap());
        assert_eq!(report.scope, CapacityScope::CapacityOnly);
        assert!(report.total_bytes.value() > 0);
        assert!(report.available_bytes <= report.free_bytes);
        assert!(report.free_bytes <= report.total_bytes);
        assert_eq!(
            report.assessment,
            assess_capacity(report.total_bytes.value(), report.available_bytes.value()).unwrap()
        );
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["scope"], "capacity_only");
        for field in ["total_bytes", "free_bytes", "available_bytes"] {
            assert!(value[field].as_str().unwrap().parse::<u64>().is_ok());
        }
    }

    /// Refuse missing paths, files, and final symlinks without creating entries.
    #[test]
    fn invalid_paths_and_final_symlinks_are_refused() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let file = parent.path().join("file");
        fs::write(&file, b"untouched").unwrap();
        let missing = parent.path().join("missing");
        assert!(matches!(
            inspect_capacity(&missing),
            Err(StorageError::Io { .. })
        ));
        assert!(!missing.exists());
        assert!(matches!(
            inspect_capacity(&file),
            Err(StorageError::InvalidPath { .. })
        ));
        assert!(matches!(
            inspect_capacity(Path::new("")),
            Err(StorageError::InvalidPath { .. })
        ));
        let alias = parent.path().join("alias");
        symlink(&directory, &alias).unwrap();
        for path in [alias.clone(), alias.join(""), alias.join(".")] {
            assert!(matches!(
                inspect_capacity(&path),
                Err(StorageError::InvalidPath { .. })
            ));
        }
        let dangling = parent.path().join("dangling");
        symlink(&missing, &dangling).unwrap();
        assert!(matches!(
            inspect_capacity(&dangling),
            Err(StorageError::InvalidPath { .. })
        ));
        assert_eq!(fs::read(&file).unwrap(), b"untouched");
        assert_eq!(fs::read_link(&alias).unwrap(), directory);
        assert_eq!(fs::read_link(&dangling).unwrap(), missing);
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 4);
    }

    /// Resolve parent aliases while inspecting the final real directory itself.
    #[test]
    fn parent_directory_symlinks_return_the_canonical_inspected_path() {
        let parent = tempfile::tempdir().unwrap();
        let actual = parent.path().join("actual");
        let child = actual.join("child");
        fs::create_dir_all(&child).unwrap();
        let alias = parent.path().join("alias");
        symlink(&actual, &alias).unwrap();
        let report = inspect_capacity(&alias.join("child")).unwrap();
        assert_eq!(report.path, child.canonicalize().unwrap());
        assert_eq!(fs::read_link(&alias).unwrap(), actual);
    }
}

/// Keep pure assessments usable even when filesystem inspection is unsupported.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_inspection_fails_without_creating_a_path() {
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("not-created");
    assert!(matches!(
        jumpstart_storage::inspect_capacity(&missing),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(!missing.exists());
    assert!(assess_capacity(1000, 500).is_ok());
}
