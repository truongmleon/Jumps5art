//! Routes Linux-only administration through a private Unix socket.
//!
//! For example, `jumpstart-server status` asks the running process whether its
//! metadata verified. Offline commands continue to use their own storage API.

use std::{error::Error, path::Path};

#[cfg(target_os = "linux")]
mod hardware;
#[cfg(target_os = "linux")]
mod linux;

pub(super) use jumpstart_server::SOCKET_PATH;

/// Serve the checked root at the selected socket until the process is stopped.
///
/// `root` selects existing metadata and `socket` selects a private runtime path.
/// Returns an error for unsupported platforms, setup failures, or listener errors.
pub(super) fn serve(root: &Path, socket: &Path) -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::serve(root, socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, socket);
        Err("the administration daemon requires Linux".into())
    }
}

/// Read the live daemon's bounded status reply from `socket`.
///
/// Returns structured JSON on success or an error if a trusted daemon could
/// not be reached. Unsupported platforms return before accessing the path.
pub(super) fn status(socket: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::status(socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        Err("the administration daemon requires Linux".into())
    }
}

/// Ask the running daemon to report current host evidence at `socket`.
///
/// Returns the live component report or an error before printing on an
/// unsupported platform, failed connection, or invalid reply.
pub(super) fn health(socket: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::health(socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        Err("the administration daemon requires Linux".into())
    }
}

/// Ask the running daemon to observe a directory for an intended volume role.
///
/// `socket` identifies the local service, `path` is the directory, and `role`
/// is primary or backup. Returns read-only preflight evidence or a refusal.
pub(super) fn volume_preflight(
    socket: &Path,
    path: &Path,
    role: &str,
) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::volume_preflight(socket, path, role);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, path, role);
        Err("the administration daemon requires Linux".into())
    }
}

/// Request that the Linux daemon persist or clear the local mutation fence.
pub(super) fn maintenance(
    socket: &Path,
    enabled: bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use jumpstart_server::local_admin::{Command, call};
        call(socket, Command::Maintenance { enabled })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, enabled);
        Err("the administration daemon requires Linux".into())
    }
}

/// Submit an operator-approved registration plan and return its saved identity.
pub(super) fn apply_volume(
    socket: &Path,
    plan_id: uuid::Uuid,
    acknowledge_unverified_encryption: bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use jumpstart_server::local_admin::{Command, call};
        call(
            socket,
            Command::VolumeApply {
                plan_id,
                acknowledge_unverified_encryption,
            },
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, plan_id, acknowledge_unverified_encryption);
        Err("the administration daemon requires Linux".into())
    }
}

/// Read a bounded device page through the authenticated local socket.
pub(super) fn list_devices(
    socket: &Path,
    after: Option<jumpstart_domain::DeviceId>,
    limit: u16,
) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use jumpstart_server::local_admin::{Command, call};
        call(socket, Command::DevicesList { after, limit })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, after, limit);
        Err("the administration daemon requires Linux".into())
    }
}

/// Revoke an installation through the authenticated local socket.
pub(super) fn revoke_device(
    socket: &Path,
    device_id: jumpstart_domain::DeviceId,
) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use jumpstart_server::local_admin::{Command, call};
        call(socket, Command::DevicesRevoke { device_id })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, device_id);
        Err("the administration daemon requires Linux".into())
    }
}
