//! Measures how much room remains on the filesystem containing a chosen folder.
//!
//! For example, before downloading a large video, Jumpstart needs to know how many
//! bytes are available and how much space its default policy would keep aside for
//! recovery. This file reads one filesystem observation and calculates those limits.
//! It does not reserve space, start or pause a job, register a storage volume, or
//! decide whether a disk is healthy. Other programs can change the free space as
//! soon as the observation is taken.

use std::path::{Path, PathBuf};

use jumpstart_domain::Counter;
use serde::Serialize;

use crate::{Result, StorageError};

const GIBIBYTE: u64 = 1024 * 1024 * 1024;
const MAX_SAFETY_RESERVE: u64 = 256 * 1024 * 1024;

/// Limits what a capacity report establishes about the inspected filesystem.
///
/// This scope supplies no device-health, storage-identity, encryption, filesystem
/// qualification, or writable-storage guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityScope {
    /// Only a point-in-time filesystem space measurement and default thresholds.
    CapacityOnly,
}

/// Default warning and recovery-space calculations for one capacity observation.
///
/// These values do not reserve bytes or authorize a producer to write. Counts use
/// exact whole bytes and serialize as decimal strings through [`Counter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CapacityAssessment {
    /// Warn when available bytes are strictly below this threshold.
    pub warning_below_bytes: Counter,
    /// Default space kept for failure recording, safe shutdown, and recovery.
    pub safety_reserve_bytes: Counter,
    /// Available bytes minus the safety reserve, with a minimum of zero.
    pub available_above_reserve_bytes: Counter,
    /// Whether available bytes are strictly below the warning threshold.
    pub below_warning_threshold: bool,
    /// Whether available bytes are less than or equal to the safety reserve.
    pub at_or_below_safety_reserve: bool,
}

/// One filesystem capacity measurement obtained through an open directory handle.
///
/// Free bytes include space that may be reserved for privileged users; available
/// bytes exclude that space. The assessment uses available bytes. This report is
/// an observation rather than a reservation, health result, or volume registration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapacityReport {
    /// Canonical absolute directory path checked against the opened handle.
    pub path: PathBuf,
    /// Explicitly restricts this report to capacity evidence.
    pub scope: CapacityScope,
    /// Total size of the containing filesystem in bytes.
    pub total_bytes: Counter,
    /// Free bytes, including space unavailable to ordinary users.
    pub free_bytes: Counter,
    /// Bytes the filesystem reports as available to ordinary users.
    pub available_bytes: Counter,
    /// Default warning and recovery-space calculations for this observation.
    pub assessment: CapacityAssessment,
}

/// Calculate the documented default warning and safety-reserve thresholds.
///
/// The warning threshold is ten percent rounded upward to whole bytes, and at
/// least 5 GiB on filesystems of at least 50 GiB. The safety reserve is one percent
/// rounded downward, capped at 256 MiB. Tiny filesystems can therefore have a
/// zero-byte reserve. The warning comparison is strict; the reserve comparison
/// includes equality. No bytes are reserved by this function.
///
/// # Arguments
///
/// * `total_bytes` — `u64` positive total filesystem size, at most `i64::MAX`.
/// * `available_bytes` — `u64` available space, no greater than `total_bytes`.
///
/// # Returns
///
/// `Result<CapacityAssessment>` containing exact whole-byte thresholds and flags.
///
/// # Errors
///
/// Returns [`StorageError::InvalidCapacity`] for a zero total, counts outside the
/// supported signed 64-bit range, or available space exceeding the total.
pub fn assess_capacity(total_bytes: u64, available_bytes: u64) -> Result<CapacityAssessment> {
    let total = capacity_counter(total_bytes)?;
    let available = capacity_counter(available_bytes)?;
    if total.value() == 0 {
        return Err(StorageError::InvalidCapacity(
            "total capacity must be positive",
        ));
    }
    if available > total {
        return Err(StorageError::InvalidCapacity(
            "available capacity exceeds total capacity",
        ));
    }

    // Integer division with rounding avoids overflowing percentage products.
    let mut warning = total_bytes.div_ceil(10);
    if total_bytes >= 50 * GIBIBYTE {
        warning = warning.max(5 * GIBIBYTE);
    }
    let reserve = (total_bytes / 100).min(MAX_SAFETY_RESERVE);
    Ok(CapacityAssessment {
        warning_below_bytes: capacity_counter(warning)?,
        safety_reserve_bytes: capacity_counter(reserve)?,
        available_above_reserve_bytes: capacity_counter(available_bytes.saturating_sub(reserve))?,
        below_warning_threshold: available_bytes < warning,
        at_or_below_safety_reserve: available_bytes <= reserve,
    })
}

/// Inspect capacity through one existing directory without changing its entries.
///
/// On Linux and macOS this opens the final directory without following a symlink,
/// obtains one filesystem-statistics observation through its handle, and checks
/// the reported byte counts before assessing them. Parent-directory symlinks are
/// allowed; the returned path is canonical. The handle pins the inspected folder,
/// but this function does not lock the filesystem against other writers.
///
/// # Arguments
///
/// * `path` — borrowed `&Path` naming an existing directory to inspect.
///
/// # Returns
///
/// `Result<CapacityReport>` with the path, measured counts, and default assessment.
///
/// # Errors
///
/// Returns [`StorageError::InvalidPath`] for an empty path, a final symlink, a
/// non-directory, or a detected path replacement; [`StorageError::InvalidCapacity`]
/// for impossible or overflowing measurements; contextual I/O errors for failed
/// filesystem operations; or [`StorageError::UnsupportedPlatform`] outside Linux
/// and macOS. It never creates directories, marker files, or databases.
pub fn inspect_capacity(path: &Path) -> Result<CapacityReport> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        inspect_supported(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        Err(StorageError::UnsupportedPlatform)
    }
}

/// Convert a byte count into the shared exact counter representation.
///
/// # Arguments
///
/// * `bytes` — `u64` candidate byte count.
///
/// # Returns
///
/// `Result<Counter>` containing the unchanged value.
///
/// # Errors
///
/// Returns [`StorageError::InvalidCapacity`] when `bytes` exceeds `i64::MAX`.
fn capacity_counter(bytes: u64) -> Result<Counter> {
    Counter::from_u64(bytes).map_err(|_| {
        StorageError::InvalidCapacity("byte count exceeds the supported counter range")
    })
}

/// Validate one raw filesystem observation and convert its blocks to bytes.
///
/// # Arguments
///
/// * `fragment_bytes` — `u64` byte size of the units used by the block counts.
/// * `total_blocks` — `u64` total filesystem blocks.
/// * `free_blocks` — `u64` free blocks, including privileged-user reserves.
/// * `available_blocks` — `u64` blocks available to ordinary users.
///
/// # Returns
///
/// `Result<(Counter, Counter, Counter)>` in total, free, available byte order.
///
/// # Errors
///
/// Rejects zero block size or total capacity, multiplication overflow, counts
/// exceeding `i64::MAX`, and observations outside `available <= free <= total`.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn capacity_from_blocks(
    fragment_bytes: u64,
    total_blocks: u64,
    free_blocks: u64,
    available_blocks: u64,
) -> Result<(Counter, Counter, Counter)> {
    if fragment_bytes == 0 {
        return Err(StorageError::InvalidCapacity(
            "filesystem block size must be positive",
        ));
    }
    let byte_count = |blocks: u64| {
        fragment_bytes
            .checked_mul(blocks)
            .ok_or(StorageError::InvalidCapacity(
                "filesystem byte count overflow",
            ))
            .and_then(capacity_counter)
    };
    let total = byte_count(total_blocks)?;
    let free = byte_count(free_blocks)?;
    let available = byte_count(available_blocks)?;
    if total.value() == 0 {
        return Err(StorageError::InvalidCapacity(
            "total capacity must be positive",
        ));
    }
    if available > free || free > total {
        return Err(StorageError::InvalidCapacity(
            "filesystem counts must satisfy available <= free <= total",
        ));
    }
    Ok((total, free, available))
}

/// Open, identify, and measure a directory using Linux/macOS filesystem calls.
///
/// # Arguments
///
/// * `path` — `&Path` of an existing directory; its final component cannot be a link.
///
/// # Returns
///
/// `Result<CapacityReport>` measured through the one opened directory handle.
///
/// # Errors
///
/// Propagates invalid paths, contextual filesystem failures, and invalid capacity
/// measurements. A canonical path that no longer identifies the handle is rejected.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_supported(path: &Path) -> Result<CapacityReport> {
    use std::fs::{self, File};
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags};

    if path.as_os_str().is_empty() {
        return Err(StorageError::InvalidPath {
            path: path.to_path_buf(),
            reason: "capacity inspection requires an existing directory",
        });
    }
    // Remove redundant trailing separators and `.` so they cannot hide a final
    // symlink from the operating system's NOFOLLOW check.
    let requested: PathBuf = path.components().collect();
    let descriptor = rustix::fs::open(
        &requested,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
            StorageError::InvalidPath {
                path: path.to_path_buf(),
                reason: "capacity inspection requires a directory, not a final symlink or file",
            }
        } else {
            capacity_io("open capacity directory", path, error.into())
        }
    })?;
    let directory = File::from(descriptor);
    let canonical = fs::canonicalize(&requested)
        .map_err(|error| capacity_io("resolve capacity directory", path, error))?;
    let opened = directory
        .metadata()
        .map_err(|error| capacity_io("identify opened capacity directory", path, error))?;
    let named = fs::symlink_metadata(&canonical)
        .map_err(|error| capacity_io("identify canonical capacity directory", &canonical, error))?;
    if !named.is_dir() || opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(StorageError::InvalidPath {
            path: path.to_path_buf(),
            reason: "directory identity changed during capacity inspection",
        });
    }
    measure_open_directory(&directory, &canonical)
}

/// Measure space on a directory the caller has already identified.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn measure_open_directory(
    directory: &impl std::os::fd::AsFd,
    path: &Path,
) -> Result<CapacityReport> {
    let stats = rustix::fs::fstatvfs(directory)
        .map_err(|error| capacity_io("measure filesystem capacity", path, error.into()))?;
    let (total_bytes, free_bytes, available_bytes) = capacity_from_blocks(
        stats.f_frsize,
        stats.f_blocks,
        stats.f_bfree,
        stats.f_bavail,
    )?;
    Ok(CapacityReport {
        path: path.to_owned(),
        scope: CapacityScope::CapacityOnly,
        total_bytes,
        free_bytes,
        available_bytes,
        assessment: assess_capacity(total_bytes.value(), available_bytes.value())?,
    })
}

/// Attach a filesystem operation and requested path to an I/O failure.
///
/// # Arguments
///
/// * `action` — `&'static str` identifying the attempted operation.
/// * `path` — `&Path` on which the operation was attempted.
/// * `source` — owned `std::io::Error` returned by the operating system.
///
/// # Returns
///
/// `StorageError::Io` preserving all three pieces of context.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn capacity_io(action: &'static str, path: &Path, source: std::io::Error) -> StorageError {
    StorageError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::capacity_from_blocks;

    /// Check block conversion, including the distinction between free and available.
    #[test]
    fn blocks_are_converted_to_exact_bytes() {
        let (total, free, available) = capacity_from_blocks(4096, 100, 20, 15).unwrap();
        assert_eq!(total.value(), 409_600);
        assert_eq!(free.value(), 81_920);
        assert_eq!(available.value(), 61_440);
        let (total, _, _) = capacity_from_blocks(1, i64::MAX as u64, 0, 0).unwrap();
        assert_eq!(total.value(), i64::MAX as u64);
    }

    /// Reject invalid block measurements before overflow or ordering can be hidden.
    #[test]
    fn impossible_or_overflowing_block_measurements_are_rejected() {
        for counts in [
            (0, 100, 10, 5),
            (4096, 0, 0, 0),
            (4096, 10, 11, 5),
            (4096, 10, 5, 6),
            (u64::MAX, 2, 0, 0),
            (1, 1, u64::MAX, 0),
            (1, 1, 1, u64::MAX),
            (1, i64::MAX as u64 + 1, 0, 0),
        ] {
            assert!(capacity_from_blocks(counts.0, counts.1, counts.2, counts.3).is_err());
        }
    }
}
