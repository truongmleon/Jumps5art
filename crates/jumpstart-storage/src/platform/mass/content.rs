//! Publishes verified objects and manifests inside a checked primary directory.
//!
//! A Mass file has a catalog identity and separate immutable content. Two
//! different files can reuse the same object when their original bytes match.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::OwnedFd,
    path::Path,
};

use jumpstart_domain::BlobId;
use rustix::fs::{
    AtFlags, Mode, OFlags, RenameFlags, ResolveFlags, fsync, mkdirat, openat2, renameat_with,
    unlinkat,
};
use uuid::Uuid;

use super::super::io;
use crate::{Result, StorageError, mass::MASS_CHUNK};

const NO_ESCAPE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

/// Write and synchronize content before the catalog can refer to its digest.
pub(super) fn publish(
    root: &OwnedFd,
    hash: BlobId,
    bytes: &[u8],
    manifest_name: &str,
    manifest: &[u8],
) -> Result<()> {
    let stage = ensure_dir(root, "staging")?;
    let objects = ensure_dir(root, "objects")?;
    let b3 = ensure_dir(&objects, "b3")?;
    let digest = hash.to_string();
    let prefix = ensure_dir(&b3, &digest[3..5])?;
    persist_file(&stage, &prefix, &digest[5..], bytes)?;
    let manifests = ensure_dir(root, "manifests")?;
    persist_file(&stage, &manifests, manifest_name, manifest)
}

/// Open and verify a single full chunk of the stored immutable object.
pub(super) fn read_chunk(
    root: &OwnedFd,
    hash: BlobId,
    total_length: u64,
    index: u32,
    expected: BlobId,
) -> Result<Vec<u8>> {
    let offset = u64::from(index) * MASS_CHUNK as u64;
    let length = total_length
        .checked_sub(offset)
        .filter(|length| *length > 0)
        .ok_or(StorageError::Mass(
            "the requested chunk is outside the file",
        ))?
        .min(MASS_CHUNK as u64) as usize;
    let digest = hash.to_string();
    let path = format!("objects/b3/{}/{}", &digest[3..5], &digest[5..]);
    let mut file = open_file(root, &path)?;
    if file
        .metadata()
        .map_err(|error| io("inspect Mass object", Path::new(&path), error))?
        .len()
        != total_length
    {
        return Err(StorageError::Mass("the stored object length changed"));
    }
    let mut bytes = vec![0; length];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|error| io("read Mass chunk", Path::new(&path), error))?;
    if BlobId::hash_bytes(&bytes) != expected {
        return Err(StorageError::Mass(
            "the stored chunk did not pass verification",
        ));
    }
    Ok(bytes)
}

/// Create one managed directory and refuse links or a nested mount in its place.
fn ensure_dir(parent: &OwnedFd, name: &str) -> Result<OwnedFd> {
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => {
            return Err(io(
                "create Mass storage directory",
                Path::new(name),
                error.into(),
            ));
        }
    }
    let directory = openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        NO_ESCAPE,
    )
    .map_err(|error| io("open Mass storage directory", Path::new(name), error.into()))?;
    fsync(parent).map_err(|error| {
        io(
            "synchronize Mass storage parent",
            Path::new(name),
            error.into(),
        )
    })?;
    Ok(directory)
}

/// Open only a regular file beneath the exact selected directory.
fn open_file(parent: &OwnedFd, name: &str) -> Result<File> {
    let file = File::from(
        openat2(
            parent,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            NO_ESCAPE,
        )
        .map_err(|error| io("open immutable Mass object", Path::new(name), error.into()))?,
    );
    if !file
        .metadata()
        .map_err(|error| io("inspect Mass file", Path::new(name), error))?
        .is_file()
    {
        return Err(StorageError::Mass("the object is not a regular file"));
    }
    Ok(file)
}

/// Synchronize and promote one staged file without replacing existing bytes.
fn persist_file(stage: &OwnedFd, parent: &OwnedFd, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = format!("mass-{}.tmp", Uuid::new_v4());
    let file = openat2(
        stage,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
        NO_ESCAPE,
    )
    .map_err(|error| {
        io(
            "create Mass staging file",
            Path::new(&temporary),
            error.into(),
        )
    })?;
    let mut file = File::from(file);
    let outcome = (|| {
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                io(
                    "synchronize Mass staging file",
                    Path::new(&temporary),
                    error,
                )
            })?;
        match renameat_with(
            stage,
            temporary.as_str(),
            parent,
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {
                let mut existing = open_file(parent, name)?;
                let mut observed = Vec::new();
                (&mut existing)
                    .take(bytes.len() as u64 + 1)
                    .read_to_end(&mut observed)
                    .map_err(|error| io("verify existing Mass object", Path::new(name), error))?;
                if observed != bytes {
                    return Err(StorageError::Mass(
                        "existing immutable content differs from the import",
                    ));
                }
                existing.sync_all().map_err(|error| {
                    io("synchronize existing Mass object", Path::new(name), error)
                })?;
            }
            Err(error) => {
                return Err(io(
                    "promote Mass file without replacement",
                    Path::new(name),
                    error.into(),
                ));
            }
        }
        fsync(parent).map_err(|error| {
            io(
                "synchronize published Mass directory",
                Path::new(name),
                error.into(),
            )
        })?;
        Ok(())
    })();
    match unlinkat(stage, temporary.as_str(), AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => {
            return Err(io(
                "clean Mass staging file",
                Path::new(&temporary),
                error.into(),
            ));
        }
    }
    fsync(stage).map_err(|error| {
        io(
            "synchronize Mass staging directory",
            Path::new(&temporary),
            error.into(),
        )
    })?;
    outcome
}
