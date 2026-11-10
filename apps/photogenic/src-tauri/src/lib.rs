//! Runs Photogenic's shared Tauri shell and registers the narrow Apple Photos bridge.

#[cfg(all(feature = "apple-app", any(target_os = "macos", target_os = "ios")))]
mod photos;

/// Open the packaged WebView on Mac, iPhone or iPad.
#[cfg(all(feature = "apple-app", any(target_os = "macos", target_os = "ios")))]
#[cfg_attr(target_os = "ios", tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![photos::photos])
        .run(tauri::generate_context!())
        .expect("could not open Photogenic");
}
