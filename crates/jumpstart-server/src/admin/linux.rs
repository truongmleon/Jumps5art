//! Runs the local administration listener and verifies both ends of each connection.
//!
//! Linux supplies the caller's UID through the socket. Jumpstart checks it before
//! reading JSON, so a claimed UID in a request cannot grant access. The same
//! check on the client side binds a response to the private directory's owner.

use std::{
    error::Error,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use jumpstart_protocol::{FRAME_PREFIX_BYTES, MAX_FRAME_BYTES, decode_body, write_frame};
use jumpstart_storage::{
    AdminMetadata, StorageError, VolumeRole, inspect_capacity, inspect_volume_candidate,
};
use rustix::{net::sockopt::socket_peercred, process::geteuid};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    runtime::Builder,
    signal::unix::{SignalKind, signal},
    time::timeout,
};

const ADMIN_VERSION: u16 = 1;
const DEADLINE: Duration = Duration::from_secs(15);

/// An explicitly versioned request for the local daemon.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u16,
    command: Command,
}

/// Commands allowed on this socket. Keeping the set closed rejects unknown actions.
#[derive(Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Status,
    Health,
    VolumePreflight { path: PathBuf, role: VolumeRole },
}

/// A framed response containing either the requested evidence or a typed refusal.
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Ok { report: Value },
    Error { message: String },
}

/// Owns the bound socket path. On shutdown it removes only its own filesystem entry.
struct BoundSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for BoundSocket {
    /// Unlink the listener's socket if its identity still matches the bound entry.
    fn drop(&mut self) {
        if let Ok(entry) = fs::symlink_metadata(&self.path)
            && entry.file_type().is_socket()
            && (entry.dev(), entry.ino()) == (self.device, self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Verify metadata and run the Tokio listener until SIGINT, SIGTERM, or an error.
///
/// The daemon refuses to run as root. A busy metadata root is already owned by
/// another operation. Other startup storage errors can be reported through the
/// local socket as recovery evidence without initializing any new metadata.
pub(super) fn serve(root: &Path, socket: &Path) -> Result<(), Box<dyn Error>> {
    let uid = geteuid();
    if uid.is_root() {
        return Err("run the daemon as its dedicated service user".into());
    }
    if socket_owner(socket)? != uid.as_raw() {
        return Err("the runtime directory must belong to the daemon user".into());
    }
    let metadata = AdminMetadata::open(root);
    if matches!(metadata, Err(StorageError::InUse)) {
        return Err(StorageError::InUse.into());
    }
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(accept(socket, uid.as_raw(), root, metadata))
}

/// Bind and handle one request at a time, with a deadline on each connection.
///
/// `metadata` keeps the verified root's lease alive across requests. `uid`
/// identifies the service user. Returns after an orderly signal or a fatal
/// listener error. Invalid or disconnected peers cannot stop the listener.
async fn accept(
    socket: &Path,
    uid: u32,
    root: &Path,
    metadata: Result<AdminMetadata, StorageError>,
) -> Result<(), Box<dyn Error>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let listener = UnixListener::bind(socket)?;
    let entry = fs::symlink_metadata(socket)?;
    let _bound = BoundSocket {
        path: socket.to_owned(),
        device: entry.dev(),
        inode: entry.ino(),
    };
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    loop {
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let Ok(peer) = socket_peercred(&stream) else {
                    continue;
                };
                if !peer.uid.is_root() && peer.uid.as_raw() != uid {
                    continue;
                }
                tokio::select! {
                    _ = terminate.recv() => return Ok(()),
                    _ = interrupt.recv() => return Ok(()),
                    _ = timeout(DEADLINE, reply(&mut stream, root, &metadata)) => {}
                }
            }
        }
    }
}

/// Decode and answer an already authenticated local peer.
///
/// `metadata` is the daemon's startup result. Returns after writing one framed
/// reply. An unsupported version, invalid frame, or peer I/O error never opens a
/// new root or changes the daemon state.
async fn reply(
    stream: &mut UnixStream,
    root: &Path,
    metadata: &Result<AdminMetadata, StorageError>,
) -> Result<(), Box<dyn Error>> {
    let request: Request = read(stream).await?;
    let reply = if request.version != ADMIN_VERSION {
        Reply::Error {
            message: "unsupported local administration version".into(),
        }
    } else {
        match request.command {
            Command::Status => Reply::Ok {
                report: match metadata
                    .as_ref()
                    .map_err(ToString::to_string)
                    .and_then(|lease| {
                        lease
                            .check_binding()
                            .map(|()| lease.report())
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(metadata) => json!({
                        "state": "metadata_verified",
                        "metadata": metadata,
                    }),
                    Err(error) => json!({
                        "state": "recovery_required",
                        "detail": error,
                    }),
                },
            },
            Command::Health => Reply::Ok {
                report: live_health(root, metadata),
            },
            Command::VolumePreflight { path, role } => match metadata {
                Ok(metadata) => {
                    match inspect_volume_candidate(&path, role, &metadata.report().root) {
                        Ok(candidate) => Reply::Ok {
                            report: json!({
                                "scope": "registration_preflight",
                                "candidate": candidate,
                                "encryption": "unknown",
                                "registered_topology": "not_checked",
                            }),
                        },
                        Err(error) => Reply::Error {
                            message: error.to_string(),
                        },
                    }
                }
                Err(_) => Reply::Error {
                    message: "verify the daemon's metadata before planning volume registration"
                        .into(),
                },
            },
        }
    };
    write(stream, &reply).await
}

/// Collect independent live evidence from the selected root and retained lease.
///
/// Capacity is measured now. Metadata evidence names the startup verification
/// and is downgraded if its file binding changes. Unknown components retain
/// explicit status so an accessible daemon cannot imply healthy hardware.
fn live_health(root: &Path, metadata: &Result<AdminMetadata, StorageError>) -> Value {
    let capacity = crate::health::capacity_component(inspect_capacity(root));
    let (metadata_evidence, jobs) = match metadata {
        Ok(lease) => match lease.check_binding() {
            Ok(()) => (
                json!({
                    "status": "verified_at_startup",
                    "verification_age_ms": lease.verification_age().as_millis().to_string(),
                    "evidence": lease.report(),
                }),
                match lease.snapshot_job_counts() {
                    Ok(Some(counts)) => json!({"status": "observed", "by_state": counts}),
                    Ok(None) => {
                        json!({"status": "unknown", "detail": "root predates the snapshot job ledger"})
                    }
                    Err(error) => json!({"status": "unknown", "detail": error.to_string()}),
                },
            ),
            Err(error) => (
                json!({"status": "unavailable", "detail": error.to_string(),
                    "next_action": "Stop the daemon and verify the mounted metadata root."}),
                json!({"status": "unknown"}),
            ),
        },
        Err(error) => (
            json!({"status": "recovery_required", "detail": error.to_string(),
                "next_action": "Inspect the root and its recovery journal before restarting."}),
            json!({"status": "unknown"}),
        ),
    };
    json!({
        "scope": "live_local_admin",
        "metadata": metadata_evidence,
        "capacity": capacity,
        "snapshot_jobs": jobs,
        "registered_volumes": {"status": "unknown"},
        "disk_hardware": {"status": "unknown"},
    })
}

/// Query the daemon only after checking the socket path and peer credentials.
///
/// `socket` is an absolute path in the daemon's private directory. Returns the
/// status report or a connection, authentication, timeout, or remote error.
pub(super) fn status(socket: &Path) -> Result<Value, Box<dyn Error>> {
    call(socket, Command::Status)
}

/// Return the current health evidence from the protected daemon socket.
pub(super) fn health(socket: &Path) -> Result<Value, Box<dyn Error>> {
    call(socket, Command::Health)
}

/// Exchange a typed, read-only registration preflight request.
///
/// `path` is the proposed root and `role` is its requested CLI role. Returns the
/// evidence collected in the daemon's Linux mount namespace.
pub(super) fn volume_preflight(
    socket: &Path,
    path: &Path,
    role: &str,
) -> Result<Value, Box<dyn Error>> {
    let role = match role {
        "primary" => VolumeRole::Primary,
        "backup" => VolumeRole::Backup,
        _ => return Err("volume role must be primary or backup".into()),
    };
    call(
        socket,
        Command::VolumePreflight {
            path: path.to_owned(),
            role,
        },
    )
}

/// Verify the selected socket and submit a single local admin command.
fn call(socket: &Path, command: Command) -> Result<Value, Box<dyn Error>> {
    let owner = socket_owner(socket)?;
    let entry = fs::symlink_metadata(socket)?;
    if owner == 0 || !entry.file_type().is_socket() || entry.uid() != owner {
        return Err("the administration path must be a socket owned by its service user".into());
    }
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(timeout_call(socket, owner, command))
}

/// Connect, verify the daemon UID, and exchange one request within the deadline.
async fn timeout_call(
    socket: &Path,
    owner: u32,
    command: Command,
) -> Result<Value, Box<dyn Error>> {
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
        match read(&mut stream).await? {
            Reply::Ok { report } => Ok(report),
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
fn socket_owner(socket: &Path) -> Result<u32, Box<dyn Error>> {
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
async fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, Box<dyn Error>> {
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
async fn write(stream: &mut UnixStream, payload: &impl Serialize) -> Result<(), Box<dyn Error>> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, payload)?;
    stream.write_all(&bytes).await?;
    Ok(())
}
