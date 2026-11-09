//! Inspects a candidate storage directory through one Linux directory handle.
//!
//! A pathname can remain after a disk is unmounted. The mount ID and device
//! numbers describe what is actually mounted at inspection time, and the inode
//! distinguishes directories on that mount. The report is preflight evidence
//! for an operator, not a durable volume identity or permission to write.

use std::{
    fs::{self, File},
    io::Read,
    os::{
        fd::OwnedFd,
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

use rustix::fs::{
    AtFlags, CWD, Dir, Mode, OFlags, ResolveFlags, StatxFlags, openat2, statat, statx,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, StorageError};

const MOUNTINFO: &str = "/proc/self/mountinfo";
const MAX_MOUNTINFO_BYTES: u64 = 1024 * 1024;

/// Intended role of the candidate. The daemon must enforce role separation at apply.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeRole {
    Primary,
    Backup,
}

/// Read-only evidence tied to one selected mount and directory.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct VolumeCandidate {
    pub path: PathBuf,
    pub role: VolumeRole,
    pub mount_id: String,
    pub device_major: u32,
    pub device_minor: u32,
    pub directory_inode: String,
    pub directory_birth: Option<String>,
    pub filesystem: String,
    pub read_only: bool,
    pub empty: bool,
    pub marker_present: bool,
    pub blocking_reasons: Vec<&'static str>,
}

/// Open `path` with no symlinks in any component and inspect its current mount.
///
/// `role` records the operator's intended use and `metadata_root` identifies an
/// existing directory the candidate must not overlap. Returns the observed
/// candidate, including any blocking conditions. Creates and writes no files.
/// A missing path, unavailable mount evidence, or unsafe traversal returns an error.
pub fn inspect_volume_candidate(
    path: &Path,
    role: VolumeRole,
    metadata_root: &Path,
) -> Result<VolumeCandidate> {
    Ok(open_volume_candidate(path, role, metadata_root)?.1)
}

/// Retain the exact opened directory alongside its current mount evidence.
pub(crate) fn open_volume_candidate(
    path: &Path,
    role: VolumeRole,
    metadata_root: &Path,
) -> Result<(OwnedFd, VolumeCandidate)> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(StorageError::InvalidVolume(
            "use an absolute path without parent traversal",
        ));
    }
    let directory = openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|error| io("open volume candidate without following links", path, error))?;
    let identity_flags = StatxFlags::MNT_ID | StatxFlags::INO;
    let stat = statx(
        &directory,
        "",
        AtFlags::EMPTY_PATH,
        identity_flags | StatxFlags::BTIME,
    )
    .map_err(|error| io("read volume mount identity", path, error))?;
    if stat.stx_mask & identity_flags.bits() != identity_flags.bits() {
        return Err(StorageError::InvalidVolume(
            "the kernel did not report mount and inode identity",
        ));
    }
    let mut mountinfo = String::new();
    File::open(MOUNTINFO)
        .and_then(|file| {
            file.take(MAX_MOUNTINFO_BYTES + 1)
                .read_to_string(&mut mountinfo)
        })
        .map_err(|error| io("read mount table", Path::new(MOUNTINFO), error))?;
    if mountinfo.len() as u64 > MAX_MOUNTINFO_BYTES {
        return Err(StorageError::InvalidVolume(
            "mount table exceeds its observation limit",
        ));
    }
    let (filesystem, read_only) = mount_details(&mountinfo, stat.stx_mnt_id)?;
    let empty = directory_is_empty(&directory, path)?;
    let marker_present = match statat(
        &directory,
        ".jumpstart-volume.json",
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(_) => true,
        Err(rustix::io::Errno::NOENT) => false,
        Err(error) => return Err(io("inspect existing volume marker", path, error)),
    };

    let mut blocking_reasons = Vec::new();
    if path.starts_with(metadata_root) || metadata_root.starts_with(path) {
        blocking_reasons.push("the candidate overlaps the metadata root");
    }
    if !matches!(filesystem, "ext4" | "xfs" | "btrfs") {
        blocking_reasons.push("the filesystem is outside the supported local storage set");
    }
    if read_only {
        blocking_reasons.push("the candidate is mounted read-only");
    }
    if marker_present {
        blocking_reasons.push("an existing volume marker requires explicit recovery or adoption");
    } else if !empty {
        blocking_reasons.push("a nonempty candidate requires explicit adoption");
    }
    let directory_birth = (stat.stx_mask & StatxFlags::BTIME.bits() != 0)
        .then(|| format!("{}:{}", stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec));
    Ok((
        directory,
        VolumeCandidate {
            path: path.to_owned(),
            role,
            mount_id: stat.stx_mnt_id.to_string(),
            device_major: stat.stx_dev_major,
            device_minor: stat.stx_dev_minor,
            directory_inode: stat.stx_ino.to_string(),
            directory_birth,
            filesystem: filesystem.to_owned(),
            read_only,
            empty,
            marker_present,
            blocking_reasons,
        },
    ))
}

/// Match an opened descriptor's mount ID to its filesystem and read-only flags.
///
/// `mountinfo` is the kernel's bounded mount table. Returns the filesystem type
/// and whether either the mount or the backing superblock is read-only.
fn mount_details(mountinfo: &str, mount_id: u64) -> Result<(&str, bool)> {
    let mount_id = mount_id.to_string();
    for line in mountinfo.lines() {
        let Some((mount, superblock)) = line.split_once(" - ") else {
            continue;
        };
        let mut mount = mount.split_ascii_whitespace();
        if mount.next() != Some(mount_id.as_str()) {
            continue;
        }
        let mount_options = mount.nth(4);
        let mut superblock = superblock.split_ascii_whitespace();
        let filesystem = superblock.next();
        let super_options = superblock.nth(1);
        if let (Some(filesystem), Some(mount_options), Some(super_options)) =
            (filesystem, mount_options, super_options)
        {
            let read_only = [mount_options, super_options]
                .into_iter()
                .any(|options| options.split(',').any(|option| option == "ro"));
            return Ok((filesystem, read_only));
        }
        break;
    }
    Err(StorageError::InvalidVolume(
        "the opened mount was absent from the mount table",
    ))
}

/// Read only enough entries to distinguish an empty directory from occupied data.
fn directory_is_empty(directory: &impl std::os::fd::AsFd, path: &Path) -> Result<bool> {
    let mut entries =
        Dir::read_from(directory).map_err(|error| io("read candidate directory", path, error))?;
    for entry in &mut entries {
        let entry = entry.map_err(|error| io("read candidate entry", path, error))?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Preserve the OS error and the operation that could not observe the candidate.
fn io(action: &'static str, path: &Path, source: impl Into<std::io::Error>) -> StorageError {
    StorageError::Io {
        action,
        path: path.to_owned(),
        source: source.into(),
    }
}

/// Resolve the kernel device numbers to an unambiguous stable filesystem UUID.
///
/// `candidate` supplies the observed block device. Returns its UUID from
/// Linux's udev aliases, or an error when exact filesystem identity is unknown.
pub(crate) fn filesystem_uuid(candidate: &VolumeCandidate) -> Result<Uuid> {
    if !matches!(candidate.filesystem.as_str(), "ext4" | "xfs") {
        return Err(StorageError::InvalidVolume(
            "registration currently requires ext4 or XFS",
        ));
    }
    let path = Path::new("/dev/disk/by-uuid");
    let mut matched = None;
    for (index, entry) in fs::read_dir(path)
        .map_err(|error| io("read filesystem UUID aliases", path, error))?
        .enumerate()
    {
        if index >= 4096 {
            return Err(StorageError::InvalidVolume(
                "filesystem UUID alias limit exceeded",
            ));
        }
        let entry = entry.map_err(|error| io("read filesystem UUID alias", path, error))?;
        if !entry
            .file_type()
            .map_err(|error| io("inspect filesystem UUID alias", &entry.path(), error))?
            .is_symlink()
        {
            continue;
        }
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io("read filesystem UUID target", &entry.path(), error)),
        };
        if !metadata.file_type().is_block_device()
            || rustix::fs::major(metadata.rdev()) != candidate.device_major
            || rustix::fs::minor(metadata.rdev()) != candidate.device_minor
        {
            continue;
        }
        let id = entry.file_name();
        let id = id
            .to_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(StorageError::InvalidVolume(
                "the matching filesystem UUID is malformed",
            ))?;
        if matched.replace(id).is_some() {
            return Err(StorageError::InvalidVolume(
                "multiple aliases identify the candidate filesystem",
            ));
        }
    }
    matched.ok_or(StorageError::InvalidVolume(
        "no stable UUID was observed for the candidate filesystem",
    ))
}
