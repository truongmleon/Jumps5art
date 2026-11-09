//! Routes signed release preflight to Linux and reports unsupported platforms.
//!
//! Update planning checks a package against the daemon's current storage
//! versions. It does not invoke an installer or execute the candidate binary.

use std::{error::Error, path::Path};

#[cfg(target_os = "linux")]
mod linux;

/// Verify an update bundle selected by `root` against the daemon at `socket`.
///
/// Returns structured plan evidence or an error. The Linux implementation
/// reads its installed trust key; unsupported platforms return before I/O.
pub(super) fn plan(root: &Path, socket: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::plan(root, socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, socket);
        Err("signed server update planning requires Linux".into())
    }
}
