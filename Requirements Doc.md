# Jumpstart Requirements

## I. System-Wide Foundation and Constraints

### Target Environment

- A headless Linux server runs one unified Rust backend daemon and primary storage.
- Five standalone client apps connect to the server: Muse, Photogenic, Mass, Bato, and Flex.
- The idle server minimizes CPU, memory, disk activity, and GPU use while preserving scheduled and background functions.

### User Model

- Jumpstart is strictly single-user.
- No accounts, roles, profiles, sharing, or other-user permissions are required.

### Client Platforms

- macOS
- Windows
- Linux
- iOS
- iPadOS
- Android

### Technology Constraints

- Client interfaces use Tauri v2.
- The server backend and Worlds CLI use Rust.

### Migration and Export

- The user can migrate to a new server or recover files without the Jumpstart apps.
- Export media and documents in their retained file formats and export associated metadata in documented.
- Export includes applicable metadata and user-created state:
  - Playlists and albums
  - Collections
  - Bookmarks and annotations
  - Reading progress and completion history
  - Manual face labels and clustering corrections
  - Manual media metadata corrections
  - Watch state
  - Worlds configurations and backups
- Provide a documented server-migration process.

### Connectivity and Offline Use

- When the server is online, its available files and media streams are accessible.
- When the server is unreachable, retrieval from the server fails without affecting files or media already downloaded to the client.
- On the same LAN, connections strictly prioritize local mDNS and direct QUIC.
- If a disk, metadata provider, downloader, or media processor is unavailable, unaffected libraries remain usable. The affected app states which actions are disabled, queued, or retryable.
- New clients provide setup and reconnect states and explain that only previously downloaded items are available offline.

### Device Pairing

- The user can approve, name, recognize, and remove their own devices.
- New devices require explicit pairing on the same local network.

### Offline Conflict Resolution

- Offline operations retain the order in which they were performed on their device.
- An offline change is applied automatically when the server copy has not changed since that device last synchronized.
- If both copies changed, the offline operation does not silently overwrite the server copy.
- Additions, bookmarks, and annotations are combined where possible.
- Conflicting file edits are preserved as separate copies.
- Conflicting renames, moves, and playlist reorders require the user to choose which version to keep.
- Deletions take precedence, but discarded changes remain visible in conflict history.
- An offline deletion immediately hides the item on that device and becomes **Pending Deletion**.
- On reconnection, Pending Deletion moves the server item to Recently Deleted and propagates to other devices.
- Each device provides a Keep local download after library deletion setting. If enabled, deleting an item from the server library retains that device’s downloaded copy as a device-only item. Otherwise, the local download is removed when the deletion synchronizes.
- Undo cancels a pending deletion, restores the item from Recently Deleted, or reverts the name change.
- Conflict resolution does not rely on client-device clocks.
- Online rename and reorder operations complete on the server or fail immediately. Offline changes complete locally as **Pending Sync**.

### History and File Lifecycle

- Activity history records uploads, additions, renames, moves, reorders, modifications, and deletions for seven days.
- History identifies the originating device and offers Undo for reversible actions.
- Unresolved conflicts remain until resolved. Their seven-day history period begins after resolution.
- History records actions but does not retain deleted file contents (deleted file contents are from **Recently Deleted**.
- Every server file has one owning app. Mass accesses other app libraries only as stated in the Mass requirements as READ only.
- Files deleted from Muse, Photogenic, Mass, Bato, or Flex enter a shared server Recently Deleted area for 30 days and remain visible within the owning app.
- Restoring an item returns its file and recoverable app metadata, annotations, bookmarks, watch state, and surviving album or collection membership.
- Permanent deletion removes the server file and its associated app metadata.
- Backup history is separate from Activity History and Recently Deleted and retains deleted or replaced backup versions for 30 days.
- Same-name uploads never silently overwrite an existing file. The user chooses **Replace**, **Keep Both**, or **Cancel**.

### App and Server Management

- One background daemon, `jumpstart-server`, manages the server database and filesystem.
- Uploads, downloads, folder creation, and deletion occur through the five client apps communicating with the daemon.
- Worlds communicates with the daemon through its CLI.
- Muse registers as an active Media Session on mobile. The other apps allow the operating system to manage P2P node lifecycles.
- Desktop Ctrl/Cmd+Z triggers the latest reversible action, including rename, move, reorder, deletion, and playlist creation or editing.

### Accessibility

- Support keyboard navigation where applicable.
- Provide screen-reader labels, text scaling, adequate contrast, reduced motion, and accessible media controls.

### Storage Capacity

- Warn before server, backup, or client storage reaches critical capacity.
- When storage is full, pause affected work safely, identify what is consuming space, and resume when space becomes available.

### Long-Running Operations

- Uploads, downloads, photo backup, disc ripping, encoding, decoding, prefetching, remote downloads, and other long-running jobs support pause and resume where the source permits it.
- A job can be canceled before completion.
- Canceling removes incomplete destination output unless the user retains a resumable job.
- Completed files in a partial playlist or batch remain available and are reported separately.
- Failed transfers, backups, metadata lookups, and processing jobs support retry where applicable.
- Completed organizational actions use Undo rather than Cancel.

### Server-Client Networking

- Use embedded Rust P2P networking through iroh or libp2p.

### External Service Availability

- An external outage never removes completed local content or previously fetched metadata.
- Failed work remains visible with its actual state.
- Resume is offered only when the source supports continuation. Otherwise, the app explains that the work must restart.

### Content Rights

- Require the user to confirm that they have the right to ingest content.
- Identify features that must be limited by region or source policy.

## II. Module Specifications

## Muse — Music Hub

### Ingestion

- Accept individual or playlist links from YouTube, YouTube Music, and Spotify.
- Download audio as `.m4a`.
- Resolve Spotify links through YouTube Music audio streams.
- A playlist import creates a same-named playlist in source order (with the audio files also downloaded individually in the library).
- Successful tracks remain in the playlist. Failed tracks appear as retryable placeholders and are not redownloaded when the import is retried.
- Report complete and partial download outcomes.
- Detect byte-identical duplicates using exact-file comparison.
- Detect the same recording across different encoding, silence, or intros using audio-content comparison.
- Present duplicate matches with **Keep Both** and **Use Existing** options.

### Metadata and Discovery

- Embed song thumbnails, artist tags, and time-synchronized lyrics from LRCLIB.
- Allow manual lyric correction and removal.
- Missing lyrics are a valid state and do not prevent import or playback.
- Search for (and download) new music through connected Spotify and YouTube accounts. When an item is found, open its link in their respective apps.
- Songs support search and sorting by Name, Last Played, and Track Artist.
- Playlists support search and sorting by Name and playlist-specific Last Played.

### Playlists

- Playlists are labeled **Server** or **This Device**.
- Server playlists synchronize linked offline copies across devices.
- This Device playlists remain local.
- The Artists tab automatically creates and updates artist playlists from Track Artist metadata. Specific Albums are compiled in sub playlists.
- Manually added artist-playlist tracks are marked as pinned and remain until manually removed.
- Playlist artwork can use an uploaded image or cover art from up to four songs.

### Client UI and Playback

- Provide an Apple Music-style interface for streaming and offline downloads.
- Support play, pause, resume, seek, and stop.
- On iOS, iPadOS, and Android, provide native lock-screen and notification media controls, playback progress, and cover artwork.
- Allow the same or different music to play on multiple devices through a user setting.
- Handoff transfers playback and controls to an available device that has the song locally or can reach the server.
- Keep playback active on the source until the destination confirms playback. A failed handoff leaves source playback unchanged.

## Photogenic — Photo Storage and AI Gallery

### Ingestion and Camera-Roll Backup

- Support manual upload and management.
- Support automatic mobile camera-roll backup under the selected Wi-Fi, mobile-data, and power settings.
- Deleting a photo from the native mobile Photos app does not delete its server copy.
- Media downloaded from Photogenic remains linked to its server item and is not uploaded again as a new camera-roll item unless deliberately duplicated or imported.
- Full photo-library permission is required to back up the complete camera roll.
- With limited photo permission, back up all authorized photos, identify that access is limited, and provide a route to system settings. Do not report the complete camera roll as backed up.
- Photogenic tracks which native Photos assets have been successfully backed up and links each one to its server item. Future backup scans skip unchanged assets already backed up.
- Camera-roll backup history is stored on the server. After app reinstallation, device replacement, or local-data loss, Photogenic scans the authorized photo library and matches assets against existing server files. Confirmed matches are linked without re-uploading; incomplete items are retried, and modified assets are uploaded as related versions.

### Live Photos

- Store and link both the `.HEIC` and `.MOV` components as one Live Photo.

### Duplicate and Similar Photo Review

- Classify matches as:
  - Exact Duplicates
  - Equivalent Copies
  - Related Edits
  - Burst or Similar Photos
- Only byte-identical files or confirmed identical decoded images may share one stored asset with multiple references.
- Equivalent copies, edits, bursts, and similar photos retain separate files and are grouped for review.
- Deletion is always a manual user choice.

### Photo and Video Editing

- Support reversible photo cropping, orientation changes, and markup.
- Editing a Live Photo creates a still-photo version and warns that motion will be removed.
- Revert restores the original `.HEIC` and `.MOV` pair.
- Support video speed changes, cropping, and trimming.
- Default video edits to **Save as New Video**.
- Replacing the original video requires explicit confirmation and retains a recoverable original for the Recently Deleted period.

### Metadata

- Store capture date, capture time, time zone, location, frame rate, and capture device when available.
- Allow reversible corrections to date, time, and location.
- Support file duplication and album creation.

### Server AI

- Run face recognition, people clustering, and semantic photo search locally on the server.
- **Merge People**, **Remove from Cluster**, and **Name Person** are reversible verified corrections.
- Future clustering respects these corrections and cannot rejoin manually separated faces unless the correction is reverted.

### Client UI

- Provide a Google Photos-style grid and timeline.
- Do not include a Create tab, automated collages, animations, or memory prompts.

## Mass — Cloud Drive and Remote Downloader

### Remote Downloader

- Support standard uploads and downloads.
- Support server-side HTTP downloads, including Mega and direct links.
- Support BitTorrent downloads with a seeding ratio of 0.
- Remote jobs retain resumable progress when the source supports it.
- Show an actionable waiting state for credentials, decryption keys, renewed links, confirmations, provider limits, or other required input.
- When a source cannot resume, retain the job and explain that retrieval must restart.

### Client Access and Virtual Drive

- Provide a Google Drive-style interface on all platforms.
- Support local downloads on mobile.
- Block search indexers from automatically retrieving virtual-drive content.
- Allow thumbnail generators to retrieve photo and document files smaller than 25 MB.
- Generate `.jpg` video thumbnails using ffmpeg.
- On macOS, Windows, and Linux, mount server storage as a native virtual drive.
- Support random-access opening through a local chunk cache without requiring the complete library to be downloaded first.
- Server saves and deletions are atomic; incomplete transfers never appear as completed files or deletions.
- Hold interrupted client saves in the local cache and retry them after reconnection.
- Expose Muse, Photogenic, Bato, and Flex files as read-only.

### Backups

- Allow the user to select another drive as backup storage for all six Jumpstart services.
- One backup drive can back up multiple primary drives.
- Each primary drive has its own backup folder.
- Primary additions, deletions, and recoveries mirror to the backup.
- Deleted or replaced backup versions remain in Backup History for 30 days before purge.
- Show the last successful backup and latest verification.
- Clearly identify missing, stale, full, or unreadable backup destinations.
- Support selected-file and selected-folder restore.
- Support complete primary-drive recovery to a replacement drive.
- After server restart, completed backup work is not repeated. Incomplete output resumes when safe or is discarded and restarted.
- Renaming a primary renames its backup immediately.
- Replacing a primary preserves the existing backup.
- Removing a primary archives its backup for 30 days and stops synchronization. Permanent deletion requires confirmation and remains reversible until expiry.

## Bato — eReader and Manga Library

### Supported Formats

- PDF
- EPUB
- CBZ manga chapters

### Ingestion

- Support direct file uploads.
- Provide a manually invoked MangaDex downloader and support similar sources.
- Package downloaded chapters as individual `.cbz` files while preserving covers and metadata.
- Preserve reading position and annotations across chapter removal, addition, replacement, or duplication.
- Visibly flag chapter gaps and replacements.
- At approximately 80% chapter progress, prefetch and append the next chapter for continuous reading.
- Prefetch respects network and storage settings.
- Label prefetch separately from explicit downloads.
- Prefetched chapters may be removed automatically when storage is needed. Explicit downloads are never removed automatically.

### Library and Reader

- Provide an Apple Books-style interface.
- Restrict the sidebar to **All**, **Want to Read**, **Finished**, **Collections**, and user-created collection names.
- Manual Finished status persists until the user changes it.
- An automatically finished ongoing series returns to in-progress when a new chapter arrives.
- If cover art is missing, render the title and author as the cover.
- Support local downloads, zoom, bookmarks, annotations, and reading progress.
- Define progress separately for fixed-page books and EPUBs.
- Support right-to-left CBZ pagination, vertical webtoon scrolling, and automatic double-page spread detection.
- Allow automatic rotation of wide landscape images, with a remembered per-page override.
- Allow the user to change reading direction and page pairing, including whether the first page stands alone.
- Anchor EPUB progress and annotations using EPUB CFI.
- Support vertical and horizontal scrolling.
- Optionally recognize text in image-only PDFs and CBZ pages so it can be selected and copied.
- Preserve native text selection for EPUB and text-based PDF content.

### Reading Progress and Temporary Browsing

- Track Saved Reading Progress separately from Current Viewing Location.
- Normal sequential reading advances Saved Reading Progress.
- Search, bookmarks, table of contents, chapter lists, and page scrubbing enter temporary browsing without changing Saved Reading Progress.
- While temporarily browsing, show **Return to [saved location]**.
- **Continue Reading from Here** deliberately replaces Saved Reading Progress.
- Closing during temporary browsing preserves Saved Reading Progress.
- Bookmarks and annotations never change progress.
- **Re-read** begins a new pass from the start, removes the active Finished status, and preserves prior completion history.
- The user may manually mark an item Finished. Reaching 100% also marks it Finished.

## Flex — Video Hub

### Ingestion and AV1 Processing

- Play movies, television shows, anime, and other video files.
- Rip optical discs from the headless server or a desktop client with an optical drive.
- Use `makemkvcon` and ffmpeg for disc ripping and AV1 encoding.
- Automatically encode uploaded or ripped video into AV1 for storage.
- Retain the original until the AV1 result is verified playable with every selected video, audio, subtitle, and chapter track.
- Do not re-encode an AV1 file when all included tracks are supported.
- Transcode AV1 for clients that cannot play it directly.

### Disc Identification and Track Selection

- For movies, use the disc volume label and track duration to propose the main movie track.
- If multiple plausible tracks or editions exist, require confirmation before encoding.
- For television, suggest episode tracks using expected runtime, disc order, volume label, and TMDB duration data.
- Keep tracks outside the normal episode-duration range available for review so shorts, specials, double-length episodes, and unusual premieres are not discarded.
- Before encoding an ambiguous television disc, let the user confirm the season, ordering scheme, specials, and episode mapping.
- Filter menu tracks and obvious non-content tracks by minimum runtime, but retain them for manual review when uncertain.
- Group director's cuts, dubbed versions, extras, trailers, and alternate aspect ratios under one title without treating them as duplicates.

### Metadata and Organization

- Parse filenames with `torrent-name-parser`.
- Use AniList for anime metadata and TMDB for movie and television metadata.
- Preserve existing thumbnails when available.
- Fetch name, date, rating, genre, description, cast, artwork, and television episode descriptions.
- Allow manual matching by TMDB or AniList ID.
- Manual matches and edits remain locked until explicitly reset.
- Use separate Anime, Movies, and TV folders.
- Anime defaults to AniList; Movies and TV default to TMDB.
- Moving an item between folders changes its default provider for future matching but does not overwrite locked manual metadata.
- Prompt before replacing an existing match.
- Reclassification does not re-upload or re-encode the media.

### Client UI and Playback

- Provide a PlayStation/Netflix-inspired interface with global search.
- Support subtitles and Blu-ray subtitle formats.
- Use GPU-accelerated AV1 playback and transcoding through libmpv.
- Remember audio and subtitle track preferences.
- Allow manual correction of language labels.
- Offer **Skip Opening** when an opening marker is available.
- Allow opening and ending markers to be corrected, disabled, or dismissed per item.
- Allow manual next-episode navigation at any time.
- Mark an episode or movie watched at its ending marker or at the end of the video or when the user marks it manually.
- Mark a season watched when all required episodes are watched or when the user marks it manually.
- Mark a series watched when all required available seasons are watched.
- New episodes return the series to in-progress.
- When no ending marker exists, use actual video completion or manual **Mark Watched**.

## Worlds — Minecraft Server CLI

### Interface and Scope

- Worlds is a headless Linux CLI without a Tauri interface.
- It manages one Minecraft: Java Edition server running Fabric.

### Server Management

- Support create, start, stop, restart, and status.
- `jumpstart-server` manages the Minecraft process and receives Worlds commands.
- Status reports **Starting**, **Running**, **Stopping**, **Backing Up**, **Restoring**, **Updating**, or **Stopped**.
- Starting an already-running server or stopping an already-stopped server returns a clear message without causing an error.
- Do not run conflicting operations simultaneously.
- Before stop or restart, optionally warn connected players with a configurable countdown.
- After an unexpected exit, retain logs and attempt no more than three automatic restarts within five minutes.
- Repeated failures stop restart attempts and require user action.

### Safe Backups

- Back up immediately before Minecraft, Fabric, Fabric API, mod, or configuration changes.
- A pre-change backup records the current Minecraft, Fabric, Fabric API, and mod configuration.
- Create a daily backup only when world or player data changed since the last successful backup.
- Keep the latest five unprotected scheduled backups.
- Protected, pre-change, selected restore-source, and temporary safety backups do not count toward the five-backup limit until protection is removed.
- During a running-server backup:
  1. Warn connected players when configured.
  2. Temporarily pause world saving.
  3. Request a complete save and wait for confirmation.
  4. Copy the world to the backup destination.
  5. Resume normal world saving.
- If save confirmation or copying fails, mark the backup unsuccessful, discard incomplete output, and always resume normal world saving.
- When the server is stopped, back up the world directly without starting it.
- A successful backup is verified readable and records its date, world name, and Minecraft/Fabric configuration.
- Protected backups are excluded from automatic cleanup.
- Low, failing, full, or disconnected backup storage produces a clear warning and does not delete protected backups.

### Restoration

- List backups with their dates, versions, and status.
- Require the Minecraft server to be stopped before restoration.
- Before replacement, create a safety backup unless the user explicitly declines.
- During restoration, temporarily protect the selected restore source and safety backup from cleanup.
- The five-backup limit may be exceeded until restoration succeeds or is canceled.
- A failed or interrupted restore leaves the existing world unchanged.
- After restoration, compare the backup's recorded Minecraft, Fabric, and mod configuration with the active configuration.
- Offer to restore the matching recorded configuration or require explicit confirmation before starting with a mismatch.

### Fabric and Mod Management

- Manage Minecraft, Fabric Loader, Fabric API, and installed Fabric mods.
- Support installing, removing, enabling, disabling, and updating mods.
- Warn when a mod appears incompatible with the selected Minecraft or Fabric version.
- If the server fails after an update, allow rollback to the previous server and mod configuration.
- Never upgrade or replace existing world data without a backup.
- Warn that removing a mod may affect blocks, items, or world data created by that mod.

### Failure and Recovery

- Report every operation as **Queued**, **Running**, **Completed**, **Failed**, or **Canceled**.
- Interrupted operations can be safely retried.
- Do not report backup, restore, update, start, or stop as successful before completion.
- After daemon or machine restart, detect interrupted operations and report their actual state.
- Failure messages include a short explanation and the next suggested action.

## III. External Service Failure Reference

The system-wide External Service Availability rules apply to every row below.

| Service or source | Unavailable features | Recovery behavior |
|---|---|---|
| YouTube / YouTube Music | Music search, link matching, and new audio or playlist downloads | Allow manual retry, reconnect an expired linked account, retain successful playlist tracks, and preserve failed entries for retry. |
| Spotify | Connected-account search and Spotify-link resolution | Ask the user to reconnect an expired account and preserve failed imports for retry. |
| LRCLIB | Lyrics and synchronized lyric timing | Import music without lyrics, retain existing lyrics, retry later automatically, and offer manual retry. |
| MangaDex and similar sources | Search, chapter lists, covers, metadata, and chapter downloads | Preserve interrupted work when resumable; otherwise explain restart. Never remove downloaded chapters because the source removed them. |
| TMDB / AniList | Movie, television, and anime identification and metadata | Keep media playable with existing local metadata. Retry later and retain manual matching through the corresponding provider ID. |
| Mega and direct HTTP hosts | New and interrupted remote downloads | Show whether a renewed link, password, account access, decryption key, confirmation, or quota reset is required. Preserve progress only when supported. |
| BitTorrent metadata sources, trackers, and peers | Torrent metadata retrieval, peer discovery, and active downloads | Keep the job visible, show waiting or availability status, retry discovery, and allow source replacement or cancellation. |
