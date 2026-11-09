//! Routes Linux-only administration through a private Unix socket.
//!
//! For example, `jumpstart-server status` asks the running process whether its
//! metadata verified. Offline commands continue to use their own storage API.

use std::{error::Error, path::Path};

#[cfg(target_os = "linux")]
mod linux;

/// Default socket under the runtime directory managed by systemd.
pub(super) const SOCKET_PATH: &str = "/run/jumpstart/admin.sock";

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
