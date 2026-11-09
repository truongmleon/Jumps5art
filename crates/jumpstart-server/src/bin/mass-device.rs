//! Bridges the Mac app and its extension to the paired Mass transport.
//!
//! The native app keeps its key in Keychain. It sends a bounded request and
//! the key through stdin to this short-lived helper, never in process arguments.

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "mass/device.rs"]
mod device;

/// Run one private helper invocation and report failures on stdout as JSON.
fn main() -> std::process::ExitCode {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return device::run();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        eprintln!("the Mass device helper is unavailable on this platform");
        std::process::ExitCode::FAILURE
    }
}
