//! Binds an approved Linux directory to a durable primary-volume identity.
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

use jumpstart_domain::ServerId;
use rusqlite::{OptionalExtension, params};
use rustix::fs::{Dir, Mode, OFlags, RenameFlags, fsync, openat, renameat_with};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AdminMetadata, io};
use crate::{
    FORMAT_VERSION, Result, StorageError, VolumeCandidate, VolumeRole,
    volume::{filesystem_uuid, open_volume_candidate},
};

const MARKER: &str = ".jumpstart-volume.json";
const MARKER_LIMIT: u64 = 2048;

/// A short-lived approval candidate held by the daemon before it accepts a write.
pub struct VolumeRegistrationPlan {
    candidate: VolumeCandidate,
    filesystem: Uuid,
}

/// A stable primary identity shared by the disk marker and the metadata journal.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredPrimary {
    pub format_version: u32,
    pub server_id: ServerId,
    pub volume_id: Uuid,
    pub primary_id: Uuid,
    pub registration_nonce: Uuid,
    pub role: VolumeRole,
}

/// The exact directory and marker expected by a persisted apply request.
struct Intent {
    path: PathBuf,
    filesystem: Uuid,
    inode: String,
    birth: String,
    marker: RegisteredPrimary,
    finalized: bool,
}

impl AdminMetadata {
    /// Validate read-only preflight evidence and return an eligible primary plan.
    pub fn plan_primary(&self, candidate: VolumeCandidate) -> Result<VolumeRegistrationPlan> {
        self.require_registration_writes()?;
        if candidate.role != VolumeRole::Primary {
            return Err(StorageError::InvalidVolume(
                "backup registration requires independent-device verification",
            ));
        }
        if let Some(reason) = candidate.blocking_reasons.first() {
            return Err(StorageError::InvalidVolume(reason));
        }
        if candidate.directory_birth.is_none() {
            return Err(StorageError::InvalidVolume(
                "directory birth time is unavailable",
            ));
        }
        let filesystem = filesystem_uuid(&candidate)?;
        self.check_new_primary(&candidate.path, filesystem)?;
        Ok(VolumeRegistrationPlan {
            candidate,
            filesystem,
        })
    }

    /// Apply a fresh plan or replay its persisted intent and return the primary IDs.
    ///
    /// The caller must authorize this operation and check the plan's expiry and
    /// encryption acknowledgement before passing it to the storage layer.
    pub fn apply_primary(
        &mut self,
        plan_id: Uuid,
        plan: Option<&VolumeRegistrationPlan>,
    ) -> Result<RegisteredPrimary> {
        self.require_registration_writes()?;
        let intent = match self.load_intent(plan_id)? {
            Some(intent) => intent,
            None => {
                let plan = plan.ok_or(StorageError::InvalidVolume(
                    "registration plan is unknown or expired; run storage register --plan again",
                ))?;
                let (_, observed) = open_volume_candidate(
                    &plan.candidate.path,
                    VolumeRole::Primary,
                    &self.report().root,
                )?;
                if observed != plan.candidate || filesystem_uuid(&observed)? != plan.filesystem {
                    return Err(StorageError::InvalidVolume(
                        "candidate changed since preflight",
                    ));
                }
                self.check_new_primary(&observed.path, plan.filesystem)?;
                let intent = Intent {
                    path: observed.path,
                    filesystem: plan.filesystem,
                    inode: observed.directory_inode,
                    birth: observed.directory_birth.ok_or(StorageError::InvalidVolume(
                        "directory birth time is unavailable",
                    ))?,
                    marker: RegisteredPrimary {
                        format_version: FORMAT_VERSION,
                        server_id: self.report().server_id,
                        volume_id: Uuid::new_v4(),
                        primary_id: Uuid::new_v4(),
                        registration_nonce: Uuid::new_v4(),
                        role: VolumeRole::Primary,
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
        self.check_binding()?;
        self.finalize_intent(plan_id, &intent)?;
        self.checkpoint_admin()?;
        Ok(intent.marker)
    }

    /// Fence registration when metadata is stale, too old, or in maintenance.
    fn require_registration_writes(&self) -> Result<()> {
        match self.maintenance()? {
            Some(false) => Ok(()),
            Some(true) => Err(StorageError::AdminUnavailable("maintenance is enabled")),
            None => Err(StorageError::AdminUnavailable(
                "migrate offline metadata before registering volumes",
            )),
        }
    }

    /// Reject overlapping roots and filesystem reuse, including unfinished intents.
    fn check_new_primary(&self, path: &Path, filesystem: Uuid) -> Result<()> {
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

    /// Load an accepted plan from the current authority's journal, when present.
    fn load_intent(&self, plan: Uuid) -> Result<Option<Intent>> {
        Ok(self
            .source
            .connection
            .query_row(
                "SELECT root_path, filesystem_uuid, directory_inode, directory_birth, volume_id,
                    primary_id, registration_nonce, registered_volume_id IS NOT NULL
             FROM volume_registrations WHERE plan_id=?1",
                [plan.as_bytes().as_slice()],
                |row| {
                    Ok(Intent {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        filesystem: Uuid::from_bytes(row.get(1)?),
                        inode: row.get(2)?,
                        birth: row.get(3)?,
                        marker: RegisteredPrimary {
                            format_version: FORMAT_VERSION,
                            server_id: self.report().server_id,
                            volume_id: Uuid::from_bytes(row.get(4)?),
                            primary_id: Uuid::from_bytes(row.get(5)?),
                            registration_nonce: Uuid::from_bytes(row.get(6)?),
                            role: VolumeRole::Primary,
                        },
                        finalized: row.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// Record the plan and the generated identities before touching the candidate.
    fn save_intent(&self, plan: Uuid, intent: &Intent) -> Result<()> {
        self.source.connection.execute(
            "INSERT INTO volume_registrations (plan_id, root_path, filesystem_uuid,
                directory_inode, directory_birth, server_id, volume_id, primary_id,
                registration_nonce) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                plan.as_bytes().as_slice(),
                intent.path.to_str(),
                intent.filesystem.as_bytes().as_slice(),
                intent.inode,
                intent.birth,
                intent.marker.server_id.as_uuid().as_bytes().as_slice(),
                intent.marker.volume_id.as_bytes().as_slice(),
                intent.marker.primary_id.as_bytes().as_slice(),
                intent.marker.registration_nonce.as_bytes().as_slice(),
            ],
        )?;
        Ok(())
    }

    /// Reopen the accepted path and verify the identity that survives a reboot.
    fn open_intent(&self, intent: &Intent) -> Result<(OwnedFd, VolumeCandidate)> {
        let (directory, candidate) =
            open_volume_candidate(&intent.path, VolumeRole::Primary, &self.report().root)?;
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

    /// Make the primary visible and mark its intent complete in one transaction.
    fn finalize_intent(&mut self, plan: Uuid, intent: &Intent) -> Result<()> {
        let tx = self.source.connection.transaction()?;
        tx.execute(
            "INSERT INTO volumes (volume_id, role, primary_id, filesystem_identity, health)
             VALUES (?1, 'primary', ?2, ?3, 'unknown')",
            params![
                intent.marker.volume_id.as_bytes().as_slice(),
                intent.marker.primary_id.as_bytes().as_slice(),
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
    expected: &RegisteredPrimary,
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
    let actual: RegisteredPrimary = serde_json::from_slice(&bytes)?;
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
            marker: RegisteredPrimary {
                format_version: FORMAT_VERSION,
                server_id: report.server_id,
                volume_id: Uuid::new_v4(),
                primary_id: Uuid::new_v4(),
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
    }
}
