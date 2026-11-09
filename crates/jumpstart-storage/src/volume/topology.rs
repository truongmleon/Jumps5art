//! Resolves Linux block devices to the disks they depend on.
//!
//! Separate partitions and encrypted mappings can share the same disk. The
//! resolver walks sysfs partitions and slave links before declaring separation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

use rustix::fs::{AtFlags, StatxFlags, statx};

use super::VolumeCandidate;
use crate::{Result, StorageError};

const SYS: &str = "/sys";
const MAX_NODES: usize = 128;

/// Tracks a kernel disk path and its shared failure domain in this observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PhysicalDisk {
    pub sysfs: PathBuf,
    pub failure_domain: PathBuf,
    hardware_id: String,
}

/// Check that ext4's mounted journal is on the data device, then resolve its disks.
///
/// Separately mounted journals and other filesystems require their own topology
/// evidence before the daemon can use them to authorize a backup destination.
pub(crate) fn protected_disks(candidate: &VolumeCandidate) -> Result<Vec<PhysicalDisk>> {
    ext4_disks(
        &candidate.filesystem,
        candidate.device_major,
        candidate.device_minor,
    )
}

/// Resolve the mounted metadata filesystem through the daemon's retained handle.
pub(crate) fn metadata_disks(directory: &impl AsFd) -> Result<Vec<PhysicalDisk>> {
    let stat = statx(directory, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(|_| unknown("metadata mount topology is unavailable"))?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(unknown("metadata mount identity is unavailable"));
    }
    let mountinfo = super::read_mountinfo()?;
    let (filesystem, _) = super::mount_details(&mountinfo, stat.stx_mnt_id)?;
    // Reuse the same internal-journal rule without reading metadata directory entries.
    ext4_disks(filesystem, stat.stx_dev_major, stat.stx_dev_minor)
}

/// Refuse any failure domain shared by the two mounted filesystems.
pub(crate) fn require_disjoint(left: &[PhysicalDisk], right: &[PhysicalDisk]) -> Result<()> {
    if left.is_empty() || right.is_empty() {
        return Err(unknown("backing disk evidence is empty"));
    }
    if left.iter().any(|a| {
        right
            .iter()
            .any(|b| a.failure_domain == b.failure_domain || a.hardware_id == b.hardware_id)
    }) {
        return Err(StorageError::InvalidVolume(
            "backup and protected storage share an underlying disk",
        ));
    }
    Ok(())
}

/// Resolve and verify the expected ext4 data device and journal before comparing it.
fn ext4_disks(filesystem: &str, major: u32, minor: u32) -> Result<Vec<PhysicalDisk>> {
    if filesystem != "ext4" {
        return Err(unknown(
            "backup separation currently requires ext4 with an internal journal",
        ));
    }
    let sys = Path::new(SYS);
    let node = device_node(sys, major, minor)?;
    let name = node
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| unknown("the filesystem block-device name is unavailable"))?;
    // An inode journal is named <data-device>-<inode>. The reserved ext4
    // journal inode is 8. A separate journal device has no inode suffix.
    if !Path::new("/proc/fs/jbd2")
        .join(format!("{name}-8/info"))
        .is_file()
    {
        return Err(unknown("an internal ext4 journal could not be verified"));
    }
    resolve(sys, major, minor)
}

/// Return the canonical sysfs block directory for a kernel device number.
fn device_node(sys: &Path, major: u32, minor: u32) -> Result<PathBuf> {
    let node = sys.join("dev/block").join(format!("{major}:{minor}"));
    fs::canonicalize(node).map_err(|_| unknown("kernel block topology is unavailable"))
}

/// Resolve all dependencies with a finite traversal and deduplicate the leaves.
fn resolve(sys: &Path, major: u32, minor: u32) -> Result<Vec<PhysicalDisk>> {
    let mut visiting = BTreeSet::new();
    let mut resolved = BTreeSet::new();
    let mut disks = BTreeMap::new();
    visit(
        sys,
        device_node(sys, major, minor)?,
        &mut visiting,
        &mut resolved,
        &mut disks,
    )?;
    if disks.is_empty() {
        return Err(unknown("no kernel-visible physical disk was found"));
    }
    Ok(disks.into_values().collect())
}

/// Walk a partition or stacked block device and record only real disk leaves.
fn visit(
    sys: &Path,
    node: PathBuf,
    visiting: &mut BTreeSet<PathBuf>,
    resolved: &mut BTreeSet<PathBuf>,
    disks: &mut BTreeMap<PathBuf, PhysicalDisk>,
) -> Result<()> {
    if resolved.contains(&node) {
        return Ok(());
    }
    if !node.starts_with(sys.join("devices"))
        || visiting.len() + resolved.len() >= MAX_NODES
        || !visiting.insert(node.clone())
    {
        return Err(unknown(
            "the block topology is invalid or exceeds its observation limit",
        ));
    }

    if node
        .join("partition")
        .try_exists()
        .map_err(|_| unknown("partition topology is unavailable"))?
    {
        let parent = node
            .parent()
            .ok_or_else(|| unknown("the partition parent is unavailable"))?;
        visit(sys, parent.to_owned(), visiting, resolved, disks)?;
    } else {
        let entries = fs::read_dir(node.join("slaves"))
            .map_err(|_| unknown("the block device's backing links are unavailable"))?;
        let mut has_slaves = false;
        for (index, entry) in entries.take(MAX_NODES + 1).enumerate() {
            if index >= MAX_NODES {
                return Err(unknown("too many block-device backing links"));
            }
            let entry = entry.map_err(|_| unknown("a block-device backing link is unreadable"))?;
            let slave = fs::canonicalize(entry.path())
                .map_err(|_| unknown("a backing block device is unavailable"))?;
            visit(sys, slave, visiting, resolved, disks)?;
            has_slaves = true;
        }
        if !has_slaves {
            let disk = disk_leaf(sys, &node)?;
            disks.insert(node.clone(), disk);
        }
    }
    visiting.remove(&node);
    resolved.insert(node);
    Ok(())
}

/// Group NVMe namespaces on a common controller and refuse virtual disk leaves.
fn disk_leaf(sys: &Path, node: &Path) -> Result<PhysicalDisk> {
    if node.starts_with(sys.join("devices/virtual")) {
        return Err(unknown("a virtual device has no observable backing disk"));
    }
    let device = fs::canonicalize(node.join("device"))
        .map_err(|_| unknown("the backing disk has no hardware device link"))?;
    let subsystem = fs::canonicalize(device.join("subsystem"))
        .map_err(|_| unknown("the backing disk transport is unavailable"))?;
    let (kind, failure_domain, id_attribute) =
        match subsystem.file_name().and_then(|name| name.to_str()) {
            Some("nvme") => {
                let transport = read_attribute(&device.join("transport"))
                    .map_err(|_| unknown("NVMe transport is unavailable"))?;
                if transport != "pcie" {
                    return Err(unknown(
                        "NVMe over a fabric does not establish local disk separation",
                    ));
                }
                ("nvme", device.clone(), device.join("subsysnqn"))
            }
            Some("scsi")
                if device.components().any(|part| {
                    part.as_os_str()
                        .to_str()
                        .is_some_and(|part| part.starts_with("ata") || part.starts_with("usb"))
                }) =>
            {
                ("scsi", node.to_owned(), device.join("wwid"))
            }
            Some("mmc") => ("mmc", node.to_owned(), device.join("cid")),
            _ => {
                return Err(unknown(
                    "the backing disk transport cannot establish physical separation",
                ));
            }
        };
    let id = read_attribute(&id_attribute)
        .map_err(|_| unknown("the backing disk hardware identifier is unavailable"))?;
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(unknown("the backing disk hardware identifier is invalid"));
    }
    Ok(PhysicalDisk {
        sysfs: node.to_owned(),
        failure_domain,
        hardware_id: format!("{kind}:{id}"),
    })
}

/// Read a small kernel attribute and discard the enclosing whitespace.
fn read_attribute(path: &Path) -> std::io::Result<String> {
    let mut output = String::new();
    File::open(path)?.take(1025).read_to_string(&mut output)?;
    if output.len() > 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oversized kernel attribute",
        ));
    }
    Ok(output.trim().to_owned())
}

/// Distinguish a lack of trustworthy topology from a known shared disk.
fn unknown(reason: &'static str) -> StorageError {
    StorageError::VolumeTopologyUnknown(reason)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// Build a sysfs SATA disk fixture and return its partition path.
    fn sata(sys: &Path, path: &str, id: &str, device: &str) -> PathBuf {
        let scsi = sys.join("bus/scsi");
        fs::create_dir_all(&scsi).unwrap();
        let disk = sys.join("devices").join(path);
        let transport = disk.parent().unwrap();
        let partition = disk.join("part");
        fs::create_dir_all(disk.join("slaves")).unwrap();
        fs::create_dir(&partition).unwrap();
        fs::write(partition.join("partition"), "1").unwrap();
        fs::write(transport.join("wwid"), id).unwrap();
        symlink(&scsi, transport.join("subsystem")).unwrap();
        symlink(transport, disk.join("device")).unwrap();
        symlink(&partition, sys.join("dev/block").join(device)).unwrap();
        partition
    }

    /// An encrypted mapping of a partition shares its disk with that partition.
    #[test]
    fn partition_and_mapper_share_a_disk_and_a_different_disk_is_allowed() {
        let sys = tempfile::tempdir().unwrap();
        let sys = sys.path();
        fs::create_dir_all(sys.join("dev/block")).unwrap();
        let first = sata(sys, "ata1/target1/block0", "naa.001", "8:1");
        sata(sys, "ata2/target2/block1", "naa.002", "8:2");
        sata(sys, "ata3/target3/block2", "naa.001", "8:3");

        let mapper = sys.join("devices/virtual/block/dm-0");
        fs::create_dir_all(mapper.join("slaves")).unwrap();
        symlink(first, mapper.join("slaves/part")).unwrap();
        symlink(mapper, sys.join("dev/block/253:0")).unwrap();

        let data = resolve(sys, 8, 1).unwrap();
        let mapped = resolve(sys, 253, 0).unwrap();
        let separate = resolve(sys, 8, 2).unwrap();
        let duplicate_id = resolve(sys, 8, 3).unwrap();
        assert!(require_disjoint(&data, &mapped).is_err());
        assert!(require_disjoint(&data, &duplicate_id).is_err());
        require_disjoint(&data, &separate).unwrap();
        assert!(resolve(sys, 7, 0).is_err());
    }
}
