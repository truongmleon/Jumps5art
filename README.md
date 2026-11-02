# Jumpstart

Jumpstart is a self-hosted library for one person and their devices. A headless Linux server stores the library. Separate apps on macOS, Windows, Linux, iOS, iPadOS, and Android open that library. There are no accounts, profiles, or sharing.

The server apps are not available to install yet. The guides below describe how the finished software is meant to be used. Behavior is defined in [Requirements Doc.md](<Requirements Doc.md>). Implementation contracts are in [Technical Design.md](<Technical Design.md>).

## Guides

| Guide | What it explains |
|---|---|
| [Getting started](<docs/Getting Started/README.md>) | Pairing, offline use, deletion, undo, jobs, and export |
| [Muse](docs/Muse/README.md) | Adding music, playback, handoff, and playlists |
| [Photogenic](docs/Photogenic/README.md) | Camera-roll backup, albums, edits, people, and search |
| [Mass](docs/Mass/README.md) | Files, remote downloads, search, and server backups |
| [Bato](docs/Bato/README.md) | Books, manga, bookmarks, and saved reading place |
| [Flex](docs/Flex/README.md) | Movies, shows, anime, disc rips, and watched state |
| [Worlds](docs/Worlds/README.md) | Running, backing up, restoring, and modding the Minecraft server |

## The apps

| App | Use it for | Everyday actions |
|---|---|---|
| **Muse** | Music | Import from files or from YouTube, YouTube Music, and Spotify links. Play, download, and keep **Server** or **This Device** playlists. Each device plays its own queue unless you hand playback to another device. |
| **Photogenic** | Photos and video clips | Upload photos or back up a phone camera roll. Review duplicates before deleting anything. Crop, rotate, and mark up photos. Search by what is in a photo, group faces, and optionally generate images with local SDXL. |
| **Mass** | Files, downloads, and backups | Upload, download, and open files. Fetch HTTP, Mega, and torrent downloads on the server. Turn on **Require VPN for BitTorrent** when torrent traffic must stay on a VPN. Search file names and document text on the server. Mount the library as a drive on desktop. Send a finished download to Muse, Photogenic, Bato, or Flex. |
| **Bato** | Books and manga | Read PDF, EPUB, and CBZ. Download chapters, bookmark, annotate, and keep a saved place separate from a quick look elsewhere in the book. |
| **Flex** | Movies, TV, and anime | Add video files or rip a disc. Flex stores a verified AV1 copy and plays it, transcoding for a device that cannot play AV1 directly. Progress and watched state sync across devices. |
| **Worlds** | Minecraft | Create, start, stop, back up, restore, and change mods for one Fabric server from the command line. |

Files from Muse, Photogenic, Bato, and Flex appear in Mass as read-only. Copy one into Mass when you want a separate file you can edit there.

## What stays safe while you use it

- A new device must be paired. Removing a device cuts off its server access immediately. Files already on that device are not erased.
- Offline edits wait as **Pending Sync**. If the server copy also changed, compatible edits combine. Incompatible edits are kept so you can apply or dismiss them. A deletion wins for the server item, and an unsynced edit of that item stays recoverable on the device.
- The same file name never silently replaces an existing file. Choose **Replace**, **Keep Both**, or **Cancel**.
- A full disk pauses the work that needs space. Other libraries keep working.
- YouTube, Spotify, LRCLIB, MangaDex, TMDB, and AniList can fail or change. Accepted files and metadata stay in the library.
- Camera-roll backup remembers what already succeeded, including after the phone app is reinstalled. Unchanged photos are not uploaded again. Deleting the server copy does not upload that same photo again unless you choose **Back Up Again**.
- AI stays on your server. Photo recognition, file search, and SDXL do not send library contents to a cloud model.

## Which document to read

| Question | Read |
|---|---|
| What should the product do? | [Requirements Doc.md](<Requirements Doc.md>). Start with “System-Wide Foundation,” then the app you care about. The external-service table is at the end. |
| How will it be built? | [Technical Design.md](<Technical Design.md>). Requirements win if the two documents disagree. |
| What commands can I run today? | [jumpstart-server commands](docs/CLI/README.md). The apps, daemon, and Worlds command are still design. |

## Development

[Why the Rust types exist](docs/Development/README.md) explains their purpose using concrete examples, starting with cancellation and `JobLifecycle`. It also covers retries, snapshots, capacity reports, IDs, counters, and messages.

Install [rustup](https://rustup.rs/). The compiler is pinned in `rust-toolchain.toml`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Shared library work is starting in `crates/`. Passing those checks does not mean a platform can run the future server, native apps, or virtual drive. The commands the `jumpstart-server` binary accepts today are listed in [docs/CLI/README.md](docs/CLI/README.md).
