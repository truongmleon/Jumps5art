//! Shared local admin messages and transport used by the daemon and both CLIs.
//!
//! A caller must connect to the private service socket and check its kernel
//! peer credentials before trusting any reply. Commands use the same bounded
//! JSON frame format on both sides.

use std::{
    error::Error,
    fs,
    net::SocketAddrV4,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use jumpstart_domain::{DeviceId, ItemId, VersionId};
use jumpstart_protocol::{FRAME_PREFIX_BYTES, MAX_FRAME_BYTES, decode_body, write_frame};
use jumpstart_storage::{
    VolumeRole,
    mass::{MASS_CHUNK, MassDestination, MassImport},
};
use rustix::net::sockopt::socket_peercred;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    runtime::Builder,
    time::timeout,
};
use uuid::Uuid;

pub const ADMIN_VERSION: u16 = 1;
pub const DEADLINE: Duration = Duration::from_secs(15);

/// An explicitly versioned request for the local daemon.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u16,
    pub command: Command,
}

/// Commands allowed on this socket. Keeping the set closed rejects unknown actions.
#[derive(Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    Health,
    Mass {
        request: MassCommand,
    },
    WorldsStatus,
    WorldsBackupList {
        limit: u16,
    },
    VolumePreflight {
        path: PathBuf,
        role: VolumeRole,
    },
    VolumeApply {
        plan_id: Uuid,
        acknowledge_unverified_encryption: bool,
    },
    PairCreate {
        interface: String,
        address: SocketAddrV4,
    },
    PairPending,
    PairDecide {
        request_id: Uuid,
        transcript: String,
        approve: bool,
    },
    DevicesList {
        after: Option<DeviceId>,
        limit: u16,
    },
    DevicesRevoke {
        device_id: DeviceId,
    },
    Maintenance {
        enabled: bool,
    },
}

/// Bounded local Mass requests shared by the file browser and virtual drive.
#[derive(Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum MassCommand {
    Primaries,
    List {
        parent: Option<ItemId>,
        after: Option<ItemId>,
    },
    Search {
        query: String,
        after: Option<ItemId>,
    },
    Lookup {
        parent: Option<ItemId>,
        name: String,
    },
    Item {
        id: ItemId,
    },
    CreateFolder {
        destination: MassDestination,
    },
    Import {
        import: MassImport,
    },
    Read {
        id: ItemId,
        version: VersionId,
        chunk: u32,
    },
}

/// A framed response containing either the requested evidence or a typed refusal.
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Ok { report: Value },
    Error { message: String },
}

/// Verify the selected socket and submit a single local admin command.
pub fn call(socket: &Path, command: Command) -> Result<Value, Box<dyn Error>> {
    Ok(call_data(socket, command, None)?.0)
}

/// Send bounded upload bytes or receive a checked Mass chunk after the frame.
pub(crate) fn call_data(
    socket: &Path,
    command: Command,
    upload: Option<&[u8]>,
) -> Result<(Value, Vec<u8>), Box<dyn Error>> {
    let owner = socket_owner(socket)?;
    let entry = fs::symlink_metadata(socket)?;
    if owner == 0 || !entry.file_type().is_socket() || entry.uid() != owner {
        return Err("the administration path must be a socket owned by its service user".into());
    }
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(timeout_call(socket, owner, command, upload))
}

/// Connect, verify the daemon UID, and exchange a frame and optional exact data.
async fn timeout_call(
    socket: &Path,
    owner: u32,
    command: Command,
    upload: Option<&[u8]>,
) -> Result<(Value, Vec<u8>), Box<dyn Error>> {
    let read_chunk = matches!(
        command,
        Command::Mass {
            request: MassCommand::Read { .. }
        }
    );
    timeout(DEADLINE, async {
        let mut stream = UnixStream::connect(socket).await?;
        if socket_peercred(&stream)?.uid.as_raw() != owner {
            return Err("administration peer UID does not match the socket owner".into());
        }
        write(
            &mut stream,
            &Request {
                version: ADMIN_VERSION,
                command,
            },
        )
        .await?;
        if let Some(bytes) = upload {
            stream.write_all(bytes).await?;
        }
        match read(&mut stream).await? {
            Reply::Ok { report } => {
                let mut bytes = Vec::new();
                if read_chunk {
                    let length = report["length"]
                        .as_u64()
                        .and_then(|length| usize::try_from(length).ok())
                        .filter(|length| (1..=MASS_CHUNK).contains(length))
                        .ok_or("the daemon returned an invalid Mass chunk length")?;
                    bytes.resize(length, 0);
                    stream.read_exact(&mut bytes).await?;
                }
                Ok((report, bytes))
            }
            Reply::Error { message } => Err(message.into()),
        }
    })
    .await?
}

/// Verify a private runtime directory and its ancestors, returning its owner UID.
///
/// A service-owned directory beneath a root-owned sticky directory such as
/// `/tmp` is suitable for a scratch run. Aliases and ancestors writable by a
/// third user are refused so the client cannot be redirected to another socket.
pub fn socket_owner(socket: &Path) -> Result<u32, Box<dyn Error>> {
    let parent = socket
        .parent()
        .filter(|_| socket.is_absolute())
        .ok_or("the administration socket must be an absolute path in a private directory")?;
    if socket.file_name().is_none() || fs::canonicalize(parent)? != parent {
        return Err("the administration socket path must use its canonical directory".into());
    }
    let directory = fs::symlink_metadata(parent)?;
    if !directory.is_dir() || directory.mode() & 0o077 != 0 {
        return Err("the administration runtime directory must be private".into());
    }
    let owner = directory.uid();
    for ancestor in parent.ancestors().skip(1) {
        let entry = fs::symlink_metadata(ancestor)?;
        let root_sticky = entry.uid() == 0 && entry.mode() & 0o1000 != 0;
        if !entry.is_dir()
            || (entry.uid() != 0 && entry.uid() != owner)
            || (entry.mode() & 0o022 != 0 && !root_sticky)
        {
            return Err("an administration socket ancestor is writable by another user".into());
        }
    }
    Ok(owner)
}

/// Read one async frame, checking its size before allocating the JSON body.
pub async fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, Box<dyn Error>> {
    let mut prefix = [0; FRAME_PREFIX_BYTES];
    stream.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err("invalid local administration frame length".into());
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(decode_body(&body)?)
}

/// Encode one bounded frame with the shared codec, then write it to the socket.
pub async fn write(
    stream: &mut (impl AsyncWrite + Unpin),
    payload: &impl Serialize,
) -> Result<(), Box<dyn Error>> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, payload)?;
    stream.write_all(&bytes).await?;
    Ok(())
}
