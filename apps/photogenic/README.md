# Photogenic on Mac, iPhone, and iPad

Photogenic uses Tauri v2 and a shared React timeline. The Swift package is a native adapter for PhotoKit on both platforms. The app opens a small, typed IPC surface for permission, visible assets, local thumbnails, resource inspection, and the local queue. Original bytes and staging paths stay in the app container.

## Run the preview

Install Node 24, the repository's Rust toolchain, and Apple's development tools. Run these commands in `apps/photogenic`.

```sh
npm ci
APPLE_SIGNING_IDENTITY=- npm run tauri -- build --debug --bundles app -- --locked
open ../../target/debug/bundle/macos/Photogenic.app
```

The development bundle targets macOS 14 or later. On iPhone or iPad, use Xcode with an iOS 17 or later simulator or a signed development device. For an Apple silicon simulator:

```sh
rustup target add aarch64-apple-ios-sim
npm run tauri -- ios init --skip-targets-install
npm run tauri -- ios build --target aarch64-sim --debug --no-sign -- --locked
```

Select **Allow Photos Access** and open an item to see its actual resource list. **Prepare Originals** copies the accessible original and companion resources into a private local package. The whole Live Photo must be available before a package appears. The local queue can remove staged copies even after the source leaves the Photos selection.

Grid previews and staging use only on-device Photos resources. If Photos must retrieve an optimized original from iCloud, make it available in Apple Photos before retrying. The timeline uses the observed creation instant in UTC and leaves the capture time zone unknown. Limited permission covers only the selected assets. Hidden and protected media are excluded.

The local queue is staging for future server ingestion. It does not establish a verified backup. Server resource resolution, resumable upload, backup receipts, automatic scans, albums, and editing are outside this preview.

## Verification

CI checks Mac app packaging, unsigned iOS simulator packaging, and a synthetic local staging fixture. The fixture checks all-or-nothing Live Photo staging, failure cleanup, resource-name isolation, retention of unknown roles, and reopening the queue. Real Photos permission, optimized-library behavior, and device signing need acceptance testing in a user session.
