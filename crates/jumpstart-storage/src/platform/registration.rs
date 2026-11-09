//! Binds an approved Linux directory to a durable volume identity.
//!
//! SQLite records what to publish before the directory is modified. The same
//! plan can resume an interruption between marker publication and catalog
//! commit without adopting someone else's directory or assigning new IDs.

use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
};

use jumpstart_domain::{Counter, ServerId};
use rusqlite::{OptionalExtension, params};
use rustix::fs::{Dir, Mode, OFlags, RenameFlags, fsync, openat, renameat_with};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AdminMetadata, io};
use crate::{
    CapacityReport, FORMAT_VERSION, Result, StorageError, VolumeCandidate, VolumeRole,
    capacity::measure_open_directory,
    volume::{
        filesystem_uuid, open_volume_candidate,
        topology::{
            HardwareDevice, hardware_devices, metadata_disks, protected_disks, require_disjoint,
        },
    },
};

const MARKER: &str = ".jumpstart-volume.json";
const MARKER_LIMIT: u64 = 2048;
const HEALTH_SAMPLE_LIMIT: i64 = 8;

/// A short-lived approval candidate held by the daemon before it accepts a write.
pub struct VolumeRegistrationPlan {
    candidate: VolumeCandidate,
    filesystem: Uuid,
}

/// Stable server and volume identities shared by the disk marker and the metadata journal.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVolume {
    pub format_version: u32,
    pub server_id: ServerId,
    pub volume_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_id: Option<Uuid>,
    pub registration_nonce: Uuid,
    pub role: VolumeRole,
}

/// Live registration evidence with counts for catalog rows outside the sample.
#[derive(Serialize)]
pub struct RegisteredVolumeHealth {
    pub pending: Counter,
    pub catalog_only: Counter,
    pub unobserved_registered: Counter,
    pub volumes: Vec<VolumeObservation>,
}

/// A bounded inventory of mounted, identity-checked hardware probe targets.
pub struct RegisteredHardwareTargets {
    pub volumes: Vec<VolumeHardwareTarget>,
    pub omitted_volumes: usize,
}

/// One volume and either its checked device nodes or a topology refusal.
pub struct VolumeHardwareTarget {
    pub volume_id: Uuid,
    pub devices: Result<Vec<HardwareDevice>>,
}

/// One registered volume's live observation, tied to its recorded ID and directory.
#[derive(Serialize)]
pub struct VolumeObservation {
    pub volume_id: Uuid,
    pub role: VolumeRole,
    pub path: PathBuf,
    #[serde(flatten)]
    pub evidence: VolumeEvidence,
}

/// Separates verified marker evidence from a read-only or unavailable directory.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VolumeEvidence {
    IdentityVerified { capacity: CapacityReport },
    ReadOnly { capacity: CapacityReport },
    Unavailable { detail: String },
}

/// The exact directory and marker expected by a persisted apply request.
struct Intent {
    path: PathBuf,
    filesystem: Uuid,
    inode: String,
    birth: String,
    marker: RegisteredVolume,
    finalized: bool,
}

impl AdminMetadata {
    /// Open a finalized primary and verify the marker before using its handle.
    pub(super) fn registered_primary_root(
        &self,
        id: Uuid,
        write: bool,
    ) -> Result<(OwnedFd, VolumeCandidate)> {
        self.check_binding()?;
        let plan = self
            .source
            .connection
            .query_row(
                "SELECT plan_id FROM volume_registrations
             WHERE primary_id=?1 AND registered_volume_id IS NOT NULL",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::Mass("select a registered primary"))?;
        let intent = self
            .load_intent(Uuid::from_bytes(plan))?
            .ok_or(StorageError::Integrity(
                "registered primary is missing its journal entry",
            ))?;
        let (directory, observed) = self.open_intent(&intent)?;
        verify_marker(&directory, &intent.path, MARKER, &intent.marker)?;
        if write && observed.read_only {
            return Err(StorageError::Mass("the primary is mounted read-only"));
        }
        Ok((directory, observed))
    }

    /// Validate read-only preflight evidence and return an eligible volume plan.
    pub fn plan_volume(&self, candidate: VolumeCandidate) -> Result<VolumeRegistrationPlan> {
        self.require_registration_writes()?;
        if let Some(reason) = candidate.blocking_reasons.first() {
            return Err(StorageError::InvalidVolume(reason));
        }
        if candidate.directory_birth.is_none() {
            return Err(StorageError::InvalidVolume(
                "directory birth time is unavailable",
            ));
        }
        let filesystem = filesystem_uuid(&candidate)?;
        self.check_new_volume(&candidate.path, filesystem)?;
        self.check_topology(&candidate)?;
        Ok(VolumeRegistrationPlan {
            candidate,
            filesystem,
        })
    }

    /// Apply a fresh plan or replay its persisted intent and return its assigned IDs.
    ///
    /// The caller must authorize this operation and check the plan's expiry and
    /// encryption acknowledgement before passing it to the storage layer.
    pub fn apply_volume(
        &mut self,
        plan_id: Uuid,
        plan: Option<&VolumeRegistrationPlan>,
    ) -> Result<RegisteredVolume> {
        self.require_registration_writes()?;
        let intent = match self.load_intent(plan_id)? {
            Some(intent) => intent,
            None => {
                let plan = plan.ok_or(StorageError::InvalidVolume(
                    "registration plan is unknown or expired; run storage register --plan again",
                ))?;
                let (_, observed) = open_volume_candidate(
                    &plan.candidate.path,
                    plan.candidate.role,
                    &self.report().root,
                )?;
                if observed != plan.candidate || filesystem_uuid(&observed)? != plan.filesystem {
                    return Err(StorageError::InvalidVolume(
                        "candidate changed since preflight",
                    ));
                }
                self.check_new_volume(&observed.path, plan.filesystem)?;
                self.check_topology(&observed)?;
                let intent = Intent {
                    path: observed.path,
                    filesystem: plan.filesystem,
                    inode: observed.directory_inode,
                    birth: observed.directory_birth.ok_or(StorageError::InvalidVolume(
                        "directory birth time is unavailable",
                    ))?,
                    marker: RegisteredVolume {
                        format_version: FORMAT_VERSION,
                        server_id: self.report().server_id,
                        volume_id: Uuid::new_v4(),
                        primary_id: (observed.role == VolumeRole::Primary).then(Uuid::new_v4),
                        registration_nonce: Uuid::new_v4(),
                        role: observed.role,
                    },
                    finalized: false,
                };
                self.ensure_writer()?;
                self.save_intent(plan_id, &intent)?;
                intent
            }
        };

        let (directory, observed) = self.open_intent(&intent)?;
        if intent.finalized {
            verify_marker(&directory, &intent.path, MARKER, &intent.marker)?;
            if self.writable {
                self.checkpoint_admin()?;
            }
            return Ok(intent.marker);
        }
        self.ensure_writer()?;
        self.checkpoint_admin()?;
        self.check_topology(&observed)?;
        publish_marker(&directory, plan_id, &intent)?;
        let (_, rebound) = self.open_intent(&intent)?;
        if observed.mount_id != rebound.mount_id
            || (observed.device_major, observed.device_minor)
                != (rebound.device_major, rebound.device_minor)
        {
            return Err(StorageError::InvalidVolume(
                "candidate mount changed during apply",
            ));
        }
        self.check_topology(&rebound)?;
        self.check_binding()?;
        self.finalize_intent(plan_id, &intent)?;
        self.checkpoint_admin()?;
        Ok(intent.marker)
    }

    /// Observe up to eight registered volumes and disclose all omitted rows.
    ///
    /// Returns no evidence on a root that predates the registration journal.
    /// A missing or substituted mount is unavailable even if another disk has
    /// appeared at its former path and has plenty of free space.
    pub fn registered_volume_health(&self) -> Result<Option<RegisteredVolumeHealth>> {
        self.check_binding()?;
        if self.report().schema_version < 5 {
            return Ok(None);
        }
        let connection = &self.source.connection;
        let (pending, catalog_only, registered): (i64, i64, i64) = connection.query_row(
            "SELECT
                (SELECT count(*) FROM volume_registrations WHERE registered_volume_id IS NULL),
                (SELECT count(*) FROM volumes AS v WHERE NOT EXISTS (
                    SELECT 1 FROM volume_registrations AS r WHERE r.registered_volume_id=v.volume_id)),
                (SELECT count(*) FROM volume_registrations WHERE registered_volume_id IS NOT NULL)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM volume_registrations WHERE registered_volume_id IS NOT NULL
             ORDER BY root_path LIMIT ?1",
        )?;
        let mut rows = statement.query([HEALTH_SAMPLE_LIMIT])?;
        let mut volumes = Vec::new();
        while let Some(row) = rows.next()? {
            let plan_id = Uuid::from_bytes(row.get(0)?);
            let intent = self.load_intent(plan_id)?.ok_or(StorageError::Integrity(
                "registered volume is missing its journal entry",
            ))?;
            let evidence = match self.observe_volume(&intent) {
                Ok((true, capacity)) => VolumeEvidence::ReadOnly { capacity },
                Ok((false, capacity)) => VolumeEvidence::IdentityVerified { capacity },
                Err(error) => VolumeEvidence::Unavailable {
                    detail: error.to_string(),
                },
            };
            volumes.push(VolumeObservation {
                volume_id: intent.marker.volume_id,
                role: intent.marker.role,
                path: intent.path,
                evidence,
            });
        }
        self.check_binding()?;
        let count = |value| {
            u64::try_from(value)
                .ok()
                .and_then(|value| Counter::from_u64(value).ok())
                .ok_or(StorageError::Integrity("invalid volume health count"))
        };
        Ok(Some(RegisteredVolumeHealth {
            pending: count(pending)?,
            catalog_only: count(catalog_only)?,
            unobserved_registered: count((registered - HEALTH_SAMPLE_LIMIT).max(0))?,
            volumes,
        }))
    }

    /// Return hardware targets from the journal and verify each mounted volume marker.
    ///
    /// The caller can poll these devices after dropping its reference to the
    /// SQLite lease. Refused or disconnected targets are reported separately.
    pub fn registered_hardware_targets(&self) -> Result<RegisteredHardwareTargets> {
        self.check_binding()?;
        if self.report().schema_version < 5 {
            return Err(StorageError::AdminUnavailable(
                "metadata predates the volume journal",
            ));
        }
        const TARGET_LIMIT: i64 = 32;
        let connection = &self.source.connection;
        let total: i64 = connection.query_row(
            "SELECT count(*) FROM volume_registrations WHERE registered_volume_id IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM volume_registrations WHERE registered_volume_id IS NOT NULL
             ORDER BY root_path LIMIT ?1",
        )?;
        let mut rows = statement.query([TARGET_LIMIT])?;
        let mut volumes = Vec::new();
        while let Some(row) = rows.next()? {
            let intent =
                self.load_intent(Uuid::from_bytes(row.get(0)?))?
                    .ok_or(StorageError::Integrity(
                        "registered volume is missing its journal entry",
                    ))?;
            volumes.push(VolumeHardwareTarget {
                volume_id: intent.marker.volume_id,
                devices: self.volume_hardware_targets(&intent),
            });
        }
        self.check_binding()?;
        Ok(RegisteredHardwareTargets {
            omitted_volumes: usize::try_from(total)
                .unwrap_or(usize::MAX)
                .saturating_sub(volumes.len()),
            volumes,
        })
    }

    /// Check the volume marker before enumerating the hardware beneath its mount.
    fn volume_hardware_targets(&self, intent: &Intent) -> Result<Vec<HardwareDevice>> {
        let (directory, candidate) = self.open_intent(intent)?;
        verify_marker(&directory, &intent.path, MARKER, &intent.marker)?;
        hardware_devices(&candidate)
    }

    /// Match the mounted marker and measure that same directory's capacity.
    fn observe_volume(&self, intent: &Intent) -> Result<(bool, CapacityReport)> {
        let (directory, candidate) = self.open_intent(intent)?;
        verify_marker(&directory, &intent.path, MARKER, &intent.marker)?;
        let capacity = measure_open_directory(&directory, &intent.path)?;
        let (_, current) = self.open_intent(intent)?;
        if candidate.mount_id != current.mount_id || candidate.read_only != current.read_only {
            return Err(StorageError::InvalidVolume(
                "the volume mount changed during inspection",
            ));
        }
        Ok((candidate.read_only, capacity))
    }

    /// Fence registration when metadata is stale, too old, or in maintenance.
    fn require_registration_writes(&self) -> Result<()> {
        let maintenance = self.maintenance()?;
        if self.report().schema_version < 6 {
            return Err(StorageError::AdminUnavailable(
                "migrate offline metadata before registering volumes",
            ));
        }
        if maintenance == Some(true) {
            return Err(StorageError::AdminUnavailable("maintenance is enabled"));
        }
        Ok(())
    }

    /// Reject overlapping roots and filesystem reuse, including unfinished intents.
    fn check_new_volume(&self, path: &Path, filesystem: Uuid) -> Result<()> {
        if path.to_str().is_none() {
            return Err(StorageError::InvalidVolume("the volume path must be UTF-8"));
        }
        let connection = &self.source.connection;
        let unbound: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM volumes AS v WHERE NOT EXISTS (
                 SELECT 1 FROM volume_registrations AS r WHERE r.registered_volume_id=v.volume_id))",
            [],
            |row| row.get(0),
        )?;
        if unbound {
            return Err(StorageError::InvalidVolume(
                "an existing catalog volume needs explicit adoption before adding volumes",
            ));
        }
        let mut statement =
            connection.prepare("SELECT root_path, filesystem_uuid FROM volume_registrations")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let existing: String = row.get(0)?;
            let existing = Path::new(&existing);
            let uuid: [u8; 16] = row.get(1)?;
            if path.starts_with(existing)
                || existing.starts_with(path)
                || filesystem == Uuid::from_bytes(uuid)
            {
                return Err(StorageError::InvalidVolume(
                    "the candidate overlaps a registered or pending volume",
                ));
            }
        }
        Ok(())
    }

    /// Verify a backup's separation from metadata and every opposite-role root.
    ///
    /// Pending and finalized registrations both count. An offline or unverifiable
    /// protected disk blocks approval because separation cannot be established.
    fn check_topology(&self, candidate: &VolumeCandidate) -> Result<()> {
        let connection = &self.source.connection;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM volume_registrations WHERE role!=?1 ORDER BY plan_id LIMIT 129",
        )?;
        let mut rows = statement.query([candidate.role.as_str()])?;
        let mut opposite = Vec::new();
        while let Some(row) = rows.next()? {
            if opposite.len() == 128 {
                return Err(StorageError::VolumeTopologyUnknown(
                    "too many registrations to verify",
                ));
            }
            opposite.push(Uuid::from_bytes(row.get(0)?));
        }
        if candidate.role == VolumeRole::Primary && opposite.is_empty() {
            return Ok(());
        }
        let disks = protected_disks(candidate)?;
        if candidate.role == VolumeRole::Backup {
            require_disjoint(&disks, &metadata_disks(&self.directory)?)?;
        }
        for plan_id in opposite {
            let intent = self.load_intent(plan_id)?.ok_or(StorageError::Integrity(
                "protected volume is missing its journal entry",
            ))?;
            let (directory, observed) = self.open_intent(&intent)?;
            if intent.finalized {
                verify_marker(&directory, &intent.path, MARKER, &intent.marker)?;
            }
            require_disjoint(&disks, &protected_disks(&observed)?)?;
        }
        Ok(())
    }

    /// Load an accepted plan from the current authority's journal, when present.
    fn load_intent(&self, plan: Uuid) -> Result<Option<Intent>> {
        let backup_column = if self.report().schema_version >= 6 {
            "role='backup'"
        } else {
            "0"
        };
        let sql = format!(
            "SELECT root_path, filesystem_uuid, directory_inode, directory_birth, volume_id,
                    primary_id, registration_nonce, registered_volume_id IS NOT NULL, {backup_column}
             FROM volume_registrations WHERE plan_id=?1"
        );
        Ok(self
            .source
            .connection
            .query_row(&sql, [plan.as_bytes().as_slice()], |row| {
                Ok(Intent {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    filesystem: Uuid::from_bytes(row.get(1)?),
                    inode: row.get(2)?,
                    birth: row.get(3)?,
                    marker: RegisteredVolume {
                        format_version: FORMAT_VERSION,
                        server_id: self.report().server_id,
                        volume_id: Uuid::from_bytes(row.get(4)?),
                        primary_id: row.get::<_, Option<[u8; 16]>>(5)?.map(Uuid::from_bytes),
                        registration_nonce: Uuid::from_bytes(row.get(6)?),
                        role: if row.get(8)? {
                            VolumeRole::Backup
                        } else {
                            VolumeRole::Primary
                        },
                    },
                    finalized: row.get(7)?,
                })
            })
            .optional()?)
    }

    /// Record the plan and the generated identities before touching the candidate.
    fn save_intent(&self, plan: Uuid, intent: &Intent) -> Result<()> {
        self.source.connection.execute(
            "INSERT INTO volume_registrations (plan_id, root_path, filesystem_uuid,
                directory_inode, directory_birth, server_id, volume_id, primary_id,
                registration_nonce, role) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                plan.as_bytes().as_slice(),
                intent.path.to_str(),
                intent.filesystem.as_bytes().as_slice(),
                intent.inode,
                intent.birth,
                intent.marker.server_id.as_uuid().as_bytes().as_slice(),
                intent.marker.volume_id.as_bytes().as_slice(),
                intent
                    .marker
                    .primary_id
                    .as_ref()
                    .map(|id| id.as_bytes().as_slice()),
                intent.marker.registration_nonce.as_bytes().as_slice(),
                intent.marker.role.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Reopen the accepted path and verify the identity that survives a reboot.
    fn open_intent(&self, intent: &Intent) -> Result<(OwnedFd, VolumeCandidate)> {
        let (directory, candidate) =
            open_volume_candidate(&intent.path, intent.marker.role, &self.report().root)?;
        if candidate.directory_inode != intent.inode
            || candidate.directory_birth.as_deref() != Some(intent.birth.as_str())
            || filesystem_uuid(&candidate)? != intent.filesystem
        {
            return Err(StorageError::InvalidVolume(
                "the accepted directory or filesystem changed",
            ));
        }
        if candidate.read_only && !intent.finalized {
            return Err(StorageError::InvalidVolume(
                "the accepted filesystem is read-only",
            ));
        }
        Ok((directory, candidate))
    }

    /// Make the volume visible and mark its intent complete in one transaction.
    fn finalize_intent(&mut self, plan: Uuid, intent: &Intent) -> Result<()> {
        let tx = self.source.connection.transaction()?;
        tx.execute(
            "INSERT INTO volumes (volume_id, role, primary_id, filesystem_identity, health)
             VALUES (?1, ?2, ?3, ?4, 'unknown')",
            params![
                intent.marker.volume_id.as_bytes().as_slice(),
                intent.marker.role.as_str(),
                intent
                    .marker
                    .primary_id
                    .as_ref()
                    .map(|id| id.as_bytes().as_slice()),
                intent.filesystem.to_string(),
            ],
        )?;
        let updated = tx.execute(
            "UPDATE volume_registrations SET registered_volume_id=volume_id
             WHERE plan_id=?1 AND registered_volume_id IS NULL",
            [plan.as_bytes().as_slice()],
        )?;
        if updated != 1 {
            return Err(StorageError::Integrity(
                "volume registration intent changed",
            ));
        }
        tx.commit()?;
        Ok(())
    }
}

/// Publish only the journal's marker and only into the accepted private root.
fn publish_marker(directory: &OwnedFd, plan: Uuid, intent: &Intent) -> Result<()> {
    let stage = format!(".jumpstart-volume-{plan}.tmp");
    let mut found_marker = false;
    let mut found_stage = false;
    let mut entries = Dir::read_from(directory)
        .map_err(|error| io("read accepted volume", &intent.path, error.into()))?;
    for entry in &mut entries {
        let entry =
            entry.map_err(|error| io("read accepted volume entry", &intent.path, error.into()))?;
        let name = entry.file_name().to_bytes();
        match name {
            b"." | b".." => {}
            name if name == MARKER.as_bytes() => found_marker = true,
            name if name == stage.as_bytes() => found_stage = true,
            _ => {
                return Err(StorageError::InvalidVolume(
                    "the accepted directory contains other data",
                ));
            }
        }
    }
    if found_marker && found_stage {
        return Err(StorageError::InvalidVolume(
            "both a marker and a stage exist; inspect them before recovery",
        ));
    }
    if !found_marker {
        if !found_stage {
            let encoded = serde_json::to_vec(&intent.marker)?;
            let file = openat(
                directory,
                stage.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| {
                io(
                    "create private volume marker stage",
                    &intent.path,
                    error.into(),
                )
            })?;
            let mut file = File::from(file);
            file.write_all(&encoded)
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    io(
                        "synchronize private volume marker stage",
                        &intent.path,
                        error,
                    )
                })?;
        } else {
            verify_marker(directory, &intent.path, &stage, &intent.marker)?
                .sync_all()
                .map_err(|error| {
                    io(
                        "synchronize retained volume marker stage",
                        &intent.path,
                        error,
                    )
                })?;
        }
        renameat_with(
            directory,
            stage.as_str(),
            directory,
            MARKER,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            io(
                "publish volume marker without replacement",
                &intent.path,
                error.into(),
            )
        })?;
    }
    verify_marker(directory, &intent.path, MARKER, &intent.marker)?
        .sync_all()
        .map_err(|error| io("synchronize published volume marker", &intent.path, error))?;
    fsync(directory).map_err(|error| io("synchronize volume directory", &intent.path, error.into()))
}

/// Check that the named file is a bounded regular marker matching the journal.
fn verify_marker(
    directory: &OwnedFd,
    path: &Path,
    name: &str,
    expected: &RegisteredVolume,
) -> Result<File> {
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io("open expected volume marker", path, error.into()))?;
    let file = File::from(file);
    if !file
        .metadata()
        .map_err(|error| io("inspect volume marker", path, error))?
        .is_file()
    {
        return Err(StorageError::InvalidVolume(
            "the volume marker is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io("read expected volume marker", path, error))?;
    if bytes.len() as u64 > MARKER_LIMIT {
        return Err(StorageError::InvalidVolume(
            "the volume marker exceeds its size limit",
        ));
    }
    let actual: RegisteredVolume = serde_json::from_slice(&bytes)?;
    if actual != *expected {
        return Err(StorageError::InvalidVolume(
            "the existing volume marker differs from the accepted identity",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backup gets a volume ID and keeps the primary identifier out of its marker.
    #[test]
    fn backup_intent_has_no_primary_id_after_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("metadata");
        let path = temp.path().join("backup");
        std::fs::create_dir(&path).unwrap();
        let report = crate::initialize(&root).unwrap();
        let plan = Uuid::new_v4();
        let intent = Intent {
            path,
            filesystem: Uuid::new_v4(),
            inode: "1".into(),
            birth: "2:0".into(),
            marker: RegisteredVolume {
                format_version: FORMAT_VERSION,
                server_id: report.server_id,
                volume_id: Uuid::new_v4(),
                primary_id: None,
                registration_nonce: Uuid::new_v4(),
                role: VolumeRole::Backup,
            },
            finalized: false,
        };
        let directory = File::open(&intent.path).unwrap().into();
        let mut lease = AdminMetadata::open(&root).unwrap();
        lease.ensure_writer().unwrap();
        lease.save_intent(plan, &intent).unwrap();
        lease.checkpoint_admin().unwrap();
        publish_marker(&directory, plan, &intent).unwrap();
        lease.finalize_intent(plan, &intent).unwrap();
        lease.checkpoint_admin().unwrap();
        drop(lease);
        let reopened = AdminMetadata::open(&root)
            .unwrap()
            .load_intent(plan)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.marker, intent.marker);
        assert!(reopened.finalized);
        let value = serde_json::to_value(reopened.marker).unwrap();
        assert_eq!(value["role"], "backup");
        assert!(value.get("primary_id").is_none());
    }

    /// Exercise journal replay and catalog publication without provisioning a real disk.
    #[test]
    fn accepts_exact_marker_again_and_finalizes_the_same_primary() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("metadata");
        let path = temp.path().join("volume");
        std::fs::create_dir(&path).unwrap();
        let report = crate::initialize(&root).unwrap();
        let directory: OwnedFd = File::open(&path).unwrap().into();
        let plan = Uuid::new_v4();
        let intent = Intent {
            path,
            filesystem: Uuid::new_v4(),
            inode: "1".into(),
            birth: "2:0".into(),
            marker: RegisteredVolume {
                format_version: FORMAT_VERSION,
                server_id: report.server_id,
                volume_id: Uuid::new_v4(),
                primary_id: Some(Uuid::new_v4()),
                registration_nonce: Uuid::new_v4(),
                role: VolumeRole::Primary,
            },
            finalized: false,
        };

        let mut lease = AdminMetadata::open(&root).unwrap();
        lease.ensure_writer().unwrap();
        lease.save_intent(plan, &intent).unwrap();
        lease.checkpoint_admin().unwrap();
        let marker_path = intent.path.join(MARKER);
        std::fs::write(&marker_path, b"foreign marker").unwrap();
        assert!(publish_marker(&directory, plan, &intent).is_err());
        assert_eq!(std::fs::read(&marker_path).unwrap(), b"foreign marker");
        std::fs::remove_file(&marker_path).unwrap();

        publish_marker(&directory, plan, &intent).unwrap();
        drop(lease);
        let mut lease = AdminMetadata::open(&root).unwrap();
        let pending = lease.load_intent(plan).unwrap().unwrap();
        assert!(!pending.finalized);
        assert_eq!(pending.marker, intent.marker);
        publish_marker(&directory, plan, &pending).unwrap();
        lease.ensure_writer().unwrap();
        lease.finalize_intent(plan, &pending).unwrap();
        lease.checkpoint_admin().unwrap();
        drop(lease);

        let lease = AdminMetadata::open(&root).unwrap();
        let receipt = lease.load_intent(plan).unwrap().unwrap();
        assert!(receipt.finalized);
        assert_eq!(receipt.marker, intent.marker);

        // Even with a valid marker, a substitute directory must not report capacity.
        let detached = temp.path().join("detached");
        std::fs::rename(&intent.path, &detached).unwrap();
        std::fs::create_dir(&intent.path).unwrap();
        std::fs::copy(detached.join(MARKER), &marker_path).unwrap();
        let report = lease.registered_volume_health().unwrap().unwrap();
        assert_eq!(report.pending, Counter::ZERO);
        assert_eq!(report.catalog_only, Counter::ZERO);
        assert_eq!(report.unobserved_registered, Counter::ZERO);
        assert_eq!(report.volumes.len(), 1);
        assert_eq!(report.volumes[0].volume_id, intent.marker.volume_id);
        assert!(matches!(
            &report.volumes[0].evidence,
            VolumeEvidence::Unavailable { detail } if detail.contains("accepted directory or filesystem changed")
        ));
    }
}
