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
};

use jumpstart_server::local_admin::{
    ADMIN_VERSION, Command, DEADLINE, Reply, Request, call, read, socket_owner, write,
};
use jumpstart_storage::{
    AdminMetadata, StorageError, VolumeRole, inspect_capacity, inspect_volume_candidate,
};
use rustix::{net::sockopt::socket_peercred, process::geteuid};
use serde_json::{Value, json};
use tokio::{
    net::{UnixListener, UnixStream},
    runtime::Builder,
    signal::unix::{SignalKind, signal},
    time::timeout,
};

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
            Command::WorldsStatus => worlds_reply(metadata, |lease| {
                let evidence = lease.worlds_catalog_status()?;
                let absent = evidence.registered_primaries == jumpstart_domain::Counter::ZERO;
                Ok(json!({
                    "scope": "worlds_catalog",
                    "lifecycle": absent.then_some("absent"),
                    "players_can_connect": absent.then_some(false),
                    "reason": if absent { "no_registered_primary" } else { "runtime_unassessed" },
                    "evidence": evidence,
                }))
            }),
            Command::WorldsBackupList { limit } => worlds_reply(metadata, |lease| {
                let index = lease.worlds_recovery_index(limit)?;
                Ok(json!({"scope": "worlds_recovery_index", "index": index}))
            }),
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

/// Query a Worlds component through the checked daemon lease or return a refusal.
///
/// The closure is called only when a verified metadata lease exists. The
/// resulting reply preserves storage refusals without exposing a raw connection.
fn worlds_reply(
    metadata: &Result<AdminMetadata, StorageError>,
    query: impl FnOnce(&AdminMetadata) -> jumpstart_storage::Result<Value>,
) -> Reply {
    let result = metadata
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|lease| query(lease).map_err(|error| error.to_string()));
    match result {
        Ok(report) => Reply::Ok { report },
        Err(message) => Reply::Error { message },
    }
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
