//! Projects the Mass catalog into a private read-only Linux FUSE filesystem.
//!
//! Inodes identify catalog items. Open file handles pin a version and request
//! verified chunks through the daemon instead of opening primary storage.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsStr,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{Duration, UNIX_EPOCH},
};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use jumpstart_domain::ItemId;
use jumpstart_server::mass_client::MassClient;
use jumpstart_storage::mass::{MASS_CHUNK, MassFile, MassItem};
use rustix::process::{getegid, geteuid};
use tokio::{
    runtime::Builder,
    signal::unix::{SignalKind, signal},
    time::sleep,
};

const TTL: Duration = Duration::ZERO;
const MAX_DIRECTORY: usize = 4096;
const MAX_INODES: usize = 65_536;
const MAX_FILES: usize = 32;
const MAX_DIR_HANDLES: usize = 32;
const MAX_READ: usize = 8 * MASS_CHUNK;

/// One immutable open file and its most recently read verified chunk.
struct PinnedFile {
    inode: INodeNo,
    item: ItemId,
    version: MassFile,
    cached: Option<(u32, Vec<u8>)>,
}

/// One entry in a stable directory enumeration.
struct DirEntry {
    inode: INodeNo,
    kind: FileType,
    name: String,
}

/// Directory state captured when Linux opens an enumeration handle.
struct Directory {
    inode: INodeNo,
    entries: Vec<DirEntry>,
}

/// Maps kernel IDs to Mass identities for the lifetime of a mount.
#[derive(Default)]
struct MountState {
    by_inode: BTreeMap<INodeNo, ItemId>,
    by_item: BTreeMap<ItemId, INodeNo>,
    files: BTreeMap<FileHandle, PinnedFile>,
    directories: BTreeMap<FileHandle, Directory>,
    next_handle: u64,
}

/// The kernel-facing adapter, with all catalog reads going through the shared client.
struct Drive {
    client: MassClient,
    state: Mutex<MountState>,
    uid: u32,
    gid: u32,
}

/// Open an owner-only mount on an empty directory and wait for a signal or unmount.
pub(super) fn run(directory: &Path, socket: PathBuf) -> Result<(), Box<dyn Error>> {
    let uid = geteuid().as_raw();
    let entry = fs::symlink_metadata(directory)?;
    if !entry.is_dir()
        || entry.uid() != uid
        || entry.mode() & 0o022 != 0
        || fs::read_dir(directory)?.next().transpose()?.is_some()
    {
        return Err(
            "select an empty directory owned by the mounting user and protected from other writers"
                .into(),
        );
    }
    let directory = fs::canonicalize(directory)?;
    let client = MassClient::new(socket);
    client.list(None, None)?;
    let mut options = Config::default();
    options.mount_options = vec![
        MountOption::FSName("Mass".into()),
        MountOption::Subtype("mass".into()),
        MountOption::RO,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::DefaultPermissions,
    ];
    let drive = Drive {
        client,
        state: Mutex::new(MountState::default()),
        uid,
        gid: getegid().as_raw(),
    };
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let session = fuser::spawn_mount(drive, &directory, &options)?;
            println!("Mounted Mass read-only at {}", directory.display());
            tokio::select! {
                _ = terminate.recv() => session.umount_and_join(),
                _ = interrupt.recv() => session.umount_and_join(),
                _ = async {
                    while !session.guard.is_finished() {
                        sleep(Duration::from_millis(200)).await;
                    }
                } => session.join(),
            }
        })?;
    Ok(())
}

impl MountState {
    /// Return a stable inode for this catalog item, allocating it on first sight.
    fn inode(&mut self, item: ItemId) -> Result<INodeNo, Errno> {
        if let Some(inode) = self.by_item.get(&item) {
            return Ok(*inode);
        }
        if self.by_item.len() >= MAX_INODES {
            return Err(Errno::EOVERFLOW);
        }
        let inode = INodeNo(self.by_item.len() as u64 + 2);
        self.by_item.insert(item, inode);
        self.by_inode.insert(inode, item);
        Ok(inode)
    }

    /// Allocate a unique opaque handle shared by file and directory operations.
    fn handle(&mut self) -> Result<FileHandle, Errno> {
        self.next_handle = self.next_handle.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        Ok(FileHandle(self.next_handle))
    }
}

impl Drive {
    /// Lock mount-local identity and handle state.
    fn state(&self) -> Result<MutexGuard<'_, MountState>, Errno> {
        self.state.lock().map_err(|_| Errno::EIO)
    }

    /// Resolve a kernel inode to the root or a previously seen catalog item.
    fn item_id(&self, inode: INodeNo) -> Result<Option<ItemId>, Errno> {
        if inode == INodeNo::ROOT {
            return Ok(None);
        }
        self.state()?
            .by_inode
            .get(&inode)
            .copied()
            .map(Some)
            .ok_or(Errno::ENOENT)
    }

    /// Read current metadata for a known inode, with no disk-path authority.
    fn item(&self, inode: INodeNo) -> Result<Option<MassItem>, Errno> {
        self.item_id(inode)?
            .map(|id| self.client.item(id).map_err(|_| Errno::EIO))
            .transpose()
    }

    /// Map catalog metadata to Linux attributes without fetching content.
    fn attrs(&self, inode: INodeNo, item: Option<&MassItem>) -> FileAttr {
        let file = item.and_then(|item| item.file.as_ref());
        let size = file.map_or(0, |file| file.byte_length.value());
        let timestamp = item
            .and_then(|item| item.created_at_ms)
            .and_then(|value| UNIX_EPOCH.checked_add(Duration::from_millis(value.value())))
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino: inode,
            size,
            blocks: size.div_ceil(512),
            atime: timestamp,
            mtime: timestamp,
            ctime: timestamp,
            crtime: timestamp,
            kind: kind(file.is_some()),
            perm: if file.is_some() { 0o400 } else { 0o500 },
            nlink: if file.is_some() { 1 } else { 2 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Open a stable directory listing and map its entries to session inodes.
    fn open_directory(&self, inode: INodeNo) -> Result<FileHandle, Errno> {
        let item = self.item(inode)?;
        if item.as_ref().is_some_and(|item| item.file.is_some()) {
            return Err(Errno::ENOTDIR);
        }
        let mut listed = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .client
                .list(item.as_ref().map(|item| item.id), after)
                .map_err(|_| Errno::EIO)?;
            if listed.len() + page.items.len() > MAX_DIRECTORY {
                return Err(Errno::EOVERFLOW);
            }
            listed.extend(page.items);
            match page.next_after {
                Some(next) if after.is_none_or(|after| next > after) => after = Some(next),
                Some(_) => return Err(Errno::EIO),
                None => break,
            }
        }
        let mut state = self.state()?;
        if state.directories.len() >= MAX_DIR_HANDLES {
            return Err(Errno::ENFILE);
        }
        let parent = item
            .and_then(|item| item.parent)
            .map(|parent| state.inode(parent))
            .transpose()?
            .unwrap_or(INodeNo::ROOT);
        let mut entries = vec![
            DirEntry {
                inode,
                kind: FileType::Directory,
                name: ".".into(),
            },
            DirEntry {
                inode: parent,
                kind: FileType::Directory,
                name: "..".into(),
            },
        ];
        for item in listed {
            entries.push(DirEntry {
                inode: state.inode(item.id)?,
                kind: kind(item.file.is_some()),
                name: item.name,
            });
        }
        let handle = state.handle()?;
        state
            .directories
            .insert(handle, Directory { inode, entries });
        Ok(handle)
    }

    /// Open one immutable file version for future arbitrary-range reads.
    fn open_file(&self, inode: INodeNo) -> Result<FileHandle, Errno> {
        let item = self.item(inode)?.ok_or(Errno::EISDIR)?;
        let version = item.file.ok_or(Errno::EISDIR)?;
        let mut state = self.state()?;
        if state.files.len() >= MAX_FILES {
            return Err(Errno::ENFILE);
        }
        let handle = state.handle()?;
        state.files.insert(
            handle,
            PinnedFile {
                inode,
                item: item.id,
                version,
                cached: None,
            },
        );
        Ok(handle)
    }
}

impl Filesystem for Drive {
    /// Look up one exact Linux filename under its catalog parent.
    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let outcome = (|| {
            let parent = self.item_id(parent)?;
            let name = name.to_str().ok_or(Errno::ENOENT)?;
            let entry = self
                .client
                .lookup(parent, name.to_owned())
                .map_err(|_| Errno::EIO)?
                .ok_or(Errno::ENOENT)?;
            let inode = self.state()?.inode(entry.id)?;
            Ok(self.attrs(inode, Some(&entry)))
        })();
        match outcome {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    /// Return current metadata for an inode.
    fn getattr(&self, _: &Request, inode: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        match self.item(inode) {
            Ok(item) => reply.attr(&TTL, &self.attrs(inode, item.as_ref())),
            Err(error) => reply.error(error),
        }
    }

    /// Pin a catalog version and disable kernel zero-fill on short reads.
    fn open(&self, _: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        match self.open_file(inode) {
            Ok(handle) => reply.opened(handle, FopenFlags::FOPEN_DIRECT_IO),
            Err(error) => reply.error(error),
        }
    }

    /// Fetch the exact requested range or surface an I/O error.
    fn read(
        &self,
        _: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let outcome = (|| {
            let mut state = self.state()?;
            let file = state
                .files
                .get_mut(&handle)
                .filter(|file| file.inode == inode)
                .ok_or(Errno::EBADF)?;
            let item = file.item;
            let version = file.version.version_id;
            read_range(file, offset, size, |index| {
                self.client
                    .read(item, version, index)
                    .map_err(|_| Errno::EIO)
            })
        })();
        match outcome {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(error),
        }
    }

    /// Release a version pin and its small in-memory cache.
    fn release(
        &self,
        _: &Request,
        _: INodeNo,
        handle: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        match self.state() {
            Ok(mut state) => {
                state.files.remove(&handle);
                reply.ok();
            }
            Err(error) => reply.error(error),
        }
    }

    /// Open a directory snapshot for repeatable kernel offsets.
    fn opendir(&self, _: &Request, inode: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        match self.open_directory(inode) {
            Ok(handle) => reply.opened(handle, FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    /// Return the next portion of an already captured directory.
    fn readdir(
        &self,
        _: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Ok(state) = self.state() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(directory) = state
            .directories
            .get(&handle)
            .filter(|directory| directory.inode == inode)
        else {
            reply.error(Errno::EBADF);
            return;
        };
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(Errno::EINVAL);
            return;
        };
        for (index, entry) in directory.entries.iter().enumerate().skip(offset) {
            if reply.add(entry.inode, index as u64 + 1, entry.kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    /// Free a directory's captured entries after Linux closes it.
    fn releasedir(
        &self,
        _: &Request,
        _: INodeNo,
        handle: FileHandle,
        _: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match self.state() {
            Ok(mut state) => {
                state.directories.remove(&handle);
                reply.ok();
            }
            Err(error) => reply.error(error),
        }
    }
}

/// Join independently verified chunks without substituting bytes after a failed read.
fn read_range(
    file: &mut PinnedFile,
    offset: u64,
    size: u32,
    mut fetch: impl FnMut(u32) -> Result<Vec<u8>, Errno>,
) -> Result<Vec<u8>, Errno> {
    if size as usize > MAX_READ {
        return Err(Errno::EINVAL);
    }
    let length = file.version.byte_length.value();
    let end = offset.saturating_add(u64::from(size)).min(length);
    let mut position = offset;
    let mut output = Vec::with_capacity(end.saturating_sub(offset) as usize);
    while position < end {
        let index = u32::try_from(position / MASS_CHUNK as u64).map_err(|_| Errno::EOVERFLOW)?;
        let base = u64::from(index) * MASS_CHUNK as u64;
        let start = (position - base) as usize;
        let count = (end - position).min((MASS_CHUNK - start) as u64) as usize;
        if file
            .cached
            .as_ref()
            .is_none_or(|(cached, _)| *cached != index)
        {
            let bytes = fetch(index)?;
            if bytes.len() as u64 != (length - base).min(MASS_CHUNK as u64) {
                return Err(Errno::EIO);
            }
            file.cached = Some((index, bytes));
        }
        let (_, bytes) = file.cached.as_ref().ok_or(Errno::EIO)?;
        output.extend_from_slice(bytes.get(start..start + count).ok_or(Errno::EIO)?);
        position += count as u64;
    }
    Ok(output)
}

/// Map the only catalog entry types this adapter exposes to the kernel.
fn kind(file: bool) -> FileType {
    if file {
        FileType::RegularFile
    } else {
        FileType::Directory
    }
}

#[cfg(test)]
mod tests {
    use jumpstart_domain::{BlobId, Counter, VersionId};

    use super::*;

    /// A read across a chunk boundary returns exact bytes or fails the whole request.
    #[test]
    fn read_spans_chunks_and_reports_unavailable_bytes() {
        let mut file = PinnedFile {
            inode: INodeNo(2),
            item: ItemId::new(),
            version: MassFile {
                version_id: VersionId::new(),
                byte_length: Counter::from_u64((MASS_CHUNK + 3) as u64).unwrap(),
                blob_id: BlobId::hash_bytes(b"fixture"),
            },
            cached: None,
        };
        let offset = (MASS_CHUNK - 2) as u64;
        let bytes = read_range(&mut file, offset, 8, |index| match index {
            0 => Ok(vec![b'a'; MASS_CHUNK]),
            1 => Ok(b"end".to_vec()),
            _ => panic!("unexpected chunk"),
        })
        .unwrap();
        assert_eq!(bytes, b"aaend");
        assert_eq!(
            read_range(&mut file, (MASS_CHUNK + 3) as u64, 20, |_| panic!(
                "EOF should not fetch"
            ))
            .unwrap(),
            b""
        );
        assert_eq!(
            read_range(&mut file, offset, 4, |index| {
                if index == 0 {
                    Ok(vec![b'a'; MASS_CHUNK])
                } else {
                    Err(Errno::EIO)
                }
            }),
            Err(Errno::EIO)
        );
    }
}
