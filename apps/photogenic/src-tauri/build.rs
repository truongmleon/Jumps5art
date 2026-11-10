//! Builds and links the shared Swift PhotoKit adapter for Apple's Tauri shell.

/// Keep the server workspace checks independent of the optional Apple app.
fn main() {
    #[cfg(all(target_os = "macos", feature = "apple-app"))]
    {
        println!("cargo:rerun-if-changed=apple/Sources");
        println!("cargo:rerun-if-changed=apple/Package.swift");
        swift_rs::SwiftLinker::new("14.0")
            .with_ios("17.0")
            .with_package("PhotogenicPhotos", "apple")
            .link();
        println!("cargo:rustc-link-lib=framework=Photos");
        tauri_build::build();
    }
}
