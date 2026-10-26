//! Checks that snapshot APIs refuse platforms without the filesystem implementation.
//!
//! A Windows caller, for example, should receive an explicit unsupported error
//! rather than a partial snapshot or newly created folder. These tests exercise
//! only that refusal; snapshot creation and verification run on Linux and macOS.

#![cfg(not(any(target_os = "linux", target_os = "macos")))]

use std::fs;

use jumpstart_storage::{StorageError, create_snapshot, verify_snapshot};

/// Requires both snapshot APIs to report the unsupported platform before creating
/// source or destination entries, preserving the caller's empty directory.
#[test]
fn snapshot_apis_refuse_unsupported_platforms_without_filesystem_changes() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let source = parent.path().join("missing-source");
    let output = parent.path().join("missing-snapshot");
    assert!(matches!(
        create_snapshot(&source, &output),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(matches!(
        verify_snapshot(&output),
        Err(StorageError::UnsupportedPlatform)
    ));
    assert!(!source.exists());
    assert!(!output.exists());
    assert_eq!(
        fs::read_dir(parent.path()).expect("parent entries").count(),
        0
    );
}
