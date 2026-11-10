//! Desktop entry point for the Apple Photogenic app.

/// Start the shared shell with the macOS adapter.
fn main() {
    #[cfg(target_os = "macos")]
    photogenic_lib::run();

    #[cfg(not(target_os = "macos"))]
    eprintln!("Photogenic desktop is currently available on macOS.");
}
