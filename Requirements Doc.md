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
- Export media and documents in their original retained formats, or documented common formats when the original is unavailable.
- Export associated metadata in documented, widely readable sidecar files.
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
- Migration and export produce a consistent, complete snapshot, identify every omitted or failed item, and leave the source unchanged until the user confirms successful recovery.

### Connectivity and Offline Use

- When the server is online, its available files and media streams are accessible.
- When the server is unreachable, retrieval from the server fails without affecting files or media already downloaded to the client.
- On the same LAN, try the direct local connection first but automatically fall back to another configured route when the local route is unresponsive.
- If a disk, metadata provider, downloader, or media processor is unavailable, unaffected libraries remain usable. The affected app states which actions are disabled, queued, or retryable.
- New clients provide setup and reconnect states and explain that only previously downloaded items are available offline.

### Device Pairing

- The user can approve, name, recognize, and remove their own devices.
- New devices require explicit pairing on the same local network.
- Removing a device immediately revokes server access and rejects its queued operations. Removal does not delete server content or claim to erase copies already stored on that device.
- If no approved device is available to approve a new one, allow pairing to be approved directly from the server. Previously revoked devices remain revoked.

### Offline Conflict Resolution

- Offline operations retain the order in which they were performed on their device.
- An offline change is applied automatically when the server copy has not changed since that device last synchronized.
- Additions, bookmarks, and annotations are combined where possible.
- Conflicting file edits are preserved as separate copies.
- Deletions take precedence, but discarded changes remain visible in conflict history.
- An offline deletion immediately hides the item on that device and becomes **Pending Deletion**.
- On reconnection, Pending Deletion moves the server item to Recently Deleted and propagates to other devices.
- Each device provides a Keep local download after library deletion setting. If enabled, deleting an item from the server library retains that device’s downloaded copy as a device-only item. Otherwise, the local download is removed when the deletion synchronizes.
- When deletion conflicts with an offline content edit, deletion wins for the server item, but the edited content remains recoverable as a device-only copy that can be re-added.
- Undo cancels a pending deletion, restores the item from Recently Deleted, or reverts the name change.
- Conflict resolution does not rely on client-device clocks.
- Online rename and reorder operations complete on the server or fail immediately. Offline changes complete locally as **Pending Sync**.
- If a device attempts to change an item that changed since it was last viewed or synchronized, combine compatible changes automatically. For incompatible changes, preserve the current server state and the conflicting device change. Continue using the server state while offering a non-blocking option to apply or dismiss the device change; never silently discard either version.

### History and File Lifecycle

- Activity history records uploads, additions, renames, moves, reorders, modifications, and deletions for seven days.
- History identifies the originating device and offers Undo for reversible actions.
- Unresolved conflicts remain until resolved. Their seven-day history period begins after resolution.
- History records actions but does not retain deleted file contents; deleted file contents are retained through **Recently Deleted**.
- Every server file has one owning app. Mass accesses other app libraries only as stated in the Mass requirements as READ only.
- Files deleted from Muse, Photogenic, Mass, Bato, or Flex enter a shared server Recently Deleted area for 30 days and remain visible within the owning app.
- Restoring an item returns its file and recoverable app metadata, annotations, bookmarks, watch state, and surviving album or collection membership.
- When restoring an item from Recently Deleted, if its original name or location is occupied, offer **Restore as Copy**, **Replace Existing**, or **Cancel**; never overwrite automatically.
- Permanent deletion removes the server file and its associated app metadata.
- Backup history is separate from Activity History and Recently Deleted and retains deleted or replaced backup versions for 30 days.
- Same-name uploads never silently overwrite an existing file. The user chooses **Replace**, **Keep Both**, or **Cancel**.

### App and Server Management

- One background daemon, `jumpstart-server`, manages the SQLite or PostgreSQL database and server filesystem.
- Uploads, downloads, folder creation, moves, and deletions occur exclusively through client requests to the daemon. Clients do not modify server storage directly.
- Worlds communicates with the daemon through its CLI.
- Muse registers as an active Media Session on mobile. The other apps allow the operating system to manage P2P node lifecycles.
- Desktop Ctrl/Cmd+Z triggers the latest reversible action, including rename, move, reorder, deletion, and playlist creation or editing.
- Ctrl/Cmd+Z applies to actions originating on the current device. Undoing another device's action requires selecting it explicitly from Activity History.

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
- Background indexing, recognition, deduplication, and metadata work yields to playback and interactive transfers, pauses under resource pressure, and resumes automatically.

### Server-Client Networking

- Use embedded Rust P2P networking through iroh or libp2p.

### External Service Availability

- An external outage never removes completed local content or previously fetched metadata.
- Failed work remains visible with its actual state.
- Resume is offered only when the source supports continuation. Otherwise, the app explains that the work must restart.
- Empty, truncated, malformed, sign-in, or otherwise invalid provider responses are failures rather than authoritative no-result or removal responses. Preserve prior content, metadata, matches, and queued work.
- Automatically retry short-lived failures, such as a lost connection, temporarily unavailable service, or provider rate limit, with increasing delays. Respect provider-requested wait periods and applicable network, power, and storage settings. Stop after a limited retry budget and notify the user only when input is required or that budget is exhausted.

### Content Rights

- Require the user to confirm that they have the right to ingest content.
- Identify features that must be limited by region or source policy.

## II. Module Specifications

## Muse — Music Hub

### Ingestion

- Accept individual or playlist links from YouTube, YouTube Music, and Spotify.
- Download audio as `.m4a`.
- Resolve Spotify links through YouTube Music audio streams.
- Match Spotify items to YouTube Music using the title, primary artist, version labels such as live or remix, and approximate duration. Import automatically only when one candidate clearly agrees. Continue the remaining import without interruption and collect ambiguous items in a dedicated review list where the user can resolve or skip several items together.
- Migrate the existing Rust music-download CLI behavior into `jumpstart-server`.
- A playlist import creates a same-named playlist in source order (with the audio files also downloaded individually in the library).
- Successful tracks remain in the playlist. Failed tracks appear as retryable placeholders and are not redownloaded when the import is retried.
- Retrying an unresolved import never substitutes a different recording for one previously selected or downloaded; a changed candidate remains unresolved for batch review.
- Report complete and partial download outcomes.
- Detect byte-identical duplicates using exact-file comparison and reuse the existing library track automatically while preserving every intended playlist position.
- Detect the same recording across different encoding, silence, or intros using audio-content comparison.
- Group possible non-identical duplicate matches into a non-blocking batch review with **Keep Both** and **Use Existing** options.
- Choosing **Use Existing** places the existing library track into every intended playlist position while preserving source order and retry placeholders.

### Metadata and Discovery

- Embed song thumbnails, artist tags, and time-synchronized lyrics from LRCLIB.
- Allow manual lyric correction and removal.
- Automatic lyric refresh never replaces manually edited or removed lyrics. Keep the manual state and expose the fetched version through a non-blocking **Review Update** action.
- Missing lyrics are a valid state and do not prevent import or playback.
- Search for and import new music within Muse through connected Spotify and YouTube accounts. Opening a result in its provider app remains optional.
- Songs support search and sorting by Name, Last Played, and Track Artist.
- Playlists support search and sorting by Name and playlist-specific Last Played.

### Playlists

- Playlists are labeled **Server** or **This Device**.
- Server playlists synchronize linked offline copies across devices.
- This Device playlists remain local.
- The Artists and Albums tabs are automatically updated views based on track metadata, not editable playlists. User-created playlists remain separate.
- Playlist artwork can use an uploaded image or cover art from up to four songs.
- Deleting a playlist or removing a track from a playlist never deletes its library audio. Deleting library audio is a separate explicit action that identifies affected playlists.

### Client UI and Playback

- Provide an Apple Music-style interface for streaming and offline downloads.
- Support play, pause, resume, seek, and stop.
- On iOS, iPadOS, and Android, provide native lock-screen and notification media controls, playback progress, and cover artwork.
- Each device has an independent playback session by default. Only explicit handoff transfers a session, and remote controls target the selected active device.
- Handoff transfers playback and controls to an available device that has the song locally or can reach the server.
- Keep playback active on the source until the destination confirms playback. A failed handoff leaves source playback unchanged.
- After successful handoff, the user may control the active player from their other connected devices.
- A successful handoff preserves the current position, queue order, shuffle and repeat state, and upcoming-track context.

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
- An authorized asset that is temporarily unavailable from cloud-optimized device storage remains **Waiting for Source** and is not counted as backed up. Retry respects the selected network and power settings.
- Deleting a linked server item suppresses automatic re-upload of its unchanged native Photos asset, including after app reinstallation, device replacement, or local asset-identifier changes. Offer **Back Up Again** explicitly; later native edits remain distinguishable as modified assets.

### Live Photos

- Store and link both the `.HEIC` and `.MOV` components as one Live Photo.
- Backup, download, move, restore, and deletion treat both Live Photo components as one atomic item. Backup is complete only after both components are safely stored.

### Duplicate and Similar Photo Review

- Classify matches as:
  - Exact Duplicates
  - Equivalent Copies
  - Related Edits
  - Burst or Similar Photos
- Only byte-identical files or confirmed identical decoded images may share one stored asset with multiple references.
- Equivalent copies, edits, bursts, and similar photos retain separate files and are grouped for review.
- Deletion is always a manual user choice.
- Deleting one reference to a shared asset preserves the asset and every remaining reference. Permanent asset deletion requires removal of all references and explicit confirmation.

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
- Removing an item from an album or deleting an album never deletes the underlying photo, video, or Live Photo.

### Server AI

- Run face recognition, people clustering, and semantic photo search locally on the server.
- **Merge People**, **Remove from Cluster**, and **Name Person** are reversible verified corrections.
- Merged and named people become verified reference profiles that improve future face matching.
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
- Before resuming, verify that the remote source is unchanged. Never append data from a changed or unverified source to a partial file; retain the job and require a clean restart.
- Show an actionable waiting state for credentials, decryption keys, renewed links, confirmations, provider limits, or other required input.
- When a source cannot resume, retain the job and explain that retrieval must restart.

### Client Access and Virtual Drive

- Provide a Google Drive-style interface on all platforms.
- Support local downloads on mobile.
- Block search indexers from automatically retrieving virtual-drive content.
- Provide cached previews to the operating system without marking the original as downloaded or retrieving a complete server file solely to create a preview.
- On macOS, Windows, and Linux, mount server storage as a native virtual drive.
- Support random-access opening through a local chunk cache without requiring the complete library to be downloaded first.
- When server content is unavailable and not fully cached, expose it as unavailable rather than as an empty, partial, or completed local file. Preserve its metadata and explain that opening requires reconnection or a completed download.
- Server saves and deletions are atomic; incomplete transfers never appear as completed files or deletions.
- When the user saves through the virtual drive while the server is unavailable, retain the saved version locally and show **Pending Sync**. If the server copy changes or is deleted before synchronization, preserve both versions rather than overwriting either one.
- Hold interrupted client saves in the local cache and retry them after reconnection.
- Unsynchronized client saves survive app, device, and server restarts. Cache cleanup never removes them; they remain visible until synchronized, exported, or explicitly discarded after a data-loss warning.
- Expose Muse, Photogenic, Bato, and Flex files as read-only.

### Backups

- Allow the user to select another drive as backup storage for all six Jumpstart services.
- One backup drive can back up multiple primary drives.
- Each primary drive has its own backup folder.
- Back up canonical app files, metadata, and the active Worlds state. Exclude backup destinations, temporary caches, prefetched-only content, and incomplete output.
- Worlds remains responsible for creating, protecting, expiring, and restoring its own recovery points. Mass backs up and restores the complete Worlds data and backup catalog as one consistent unit.
- Primary additions, deletions, and recoveries mirror to the backup.
- Show the last successful backup and latest verification.
- Clearly identify missing, stale, full, or unreadable backup destinations.
- Each recovery point contains a consistent set of file contents and associated app metadata, including organization, annotations, progress, and other user-created state. Disclose any excluded state before backup.
- Support selected-file and selected-folder restore.
- Support complete primary-drive recovery to a replacement drive.
- Before a restore replaces or removes current data, show the affected items and retain the pre-restore state until the restored data is verified. A failed or interrupted restore leaves the prior state recoverable.
- After server restart, completed backup work is not repeated. Incomplete output resumes when safe or is discarded and restarted.
- Recognize a previously registered primary automatically using stable identity rather than its name or connection location, while preserving its Backup History. If that identity is missing, duplicated, or conflicts with another connected primary, do not synchronize until the user selects the correct primary.
- When a replacement or empty primary is connected to an existing backup, require **Recover Existing Primary** or **Start as New Primary**. Do not mirror deletions into the existing backup before that choice and successful recovery verification.
- Removing a primary archives its backup for 30 days and stops synchronization. The archive remains reversible until expiry, but the user may permanently purge it earlier after explicit confirmation.

## Bato — eReader and Manga Library

### Supported Formats

- PDF
- EPUB
- CBZ manga chapters

### Ingestion

- Support direct file uploads.
- Provide a manually invoked MangaDex downloader and support similar sources.
- Package downloaded chapters as individual `.cbz` files while preserving covers and metadata.
- Identify chapters by source, language, edition or scan group, volume, and chapter number. Alternate releases remain separate and enter the default reading sequence only when selected; navigation and prefetch follow that sequence.
- Preserve reading position and annotations across chapter removal, addition, replacement, or duplication.
- If replacement invalidates a saved position, bookmark, or annotation anchor, preserve it as unresolved against the prior version. Never silently discard it or attach it to different content; allow the user to relocate or delete it.
- Visibly flag chapter gaps and replacements.
- At approximately 80% chapter progress, prefetch at most the next chapter in the selected sequence on the device currently reading. Retrieve a source chapter onto the server only once, while allowing each reading device to keep its own local copy as needed. Do not alter the reading sequence.
- Prefetch respects network and storage settings.
- Label prefetch separately from explicit downloads.
- Explicitly downloading a complete, verified prefetched chapter promotes the existing copy without downloading it again.
- Store chapter identity, bookmarks, annotations, unresolved anchors, and reading progress independently from cached chapter files so they survive cache cleanup.
- Prefetched chapters may be removed automatically when storage is needed only when they are closed and the same release can be fetched again. When downloaded again, restore its saved state automatically. If the same release is unavailable or changed, keep the saved state unresolved rather than attaching it to different content. Explicit downloads are never removed automatically.
- Do not automatically remove a prefetched chapter while it is open in the reader. It becomes eligible for cleanup after the reader closes it.

### Library and Reader

- Provide an Apple Books-style interface.
- Restrict the sidebar to **All**, **Want to Read**, **Finished**, **Collections**, and user-created collection names.
- Manual Finished status persists until the user changes it.
- An automatically finished ongoing series returns to in-progress when a new chapter arrives.
- If cover art is missing, render the title and author as the cover.
- Support local downloads, zoom, bookmarks, annotations, and reading progress.
- Define progress separately for fixed-page books and EPUBs.
- Support right-to-left CBZ pagination, vertical webtoon scrolling, and automatic double-page spread detection.
- For wide images, automatically choose fit, spread, or split-page presentation according to the active reading mode. Rotate only when orientation metadata requires correction or the user requests it, and remember overrides per item.
- Allow the user to change reading direction and page pairing, including whether the first page stands alone.
- Anchor EPUB progress and annotations using EPUB CFI.
- Support vertical and horizontal scrolling.
- Optionally recognize text in image-only PDFs and CBZ pages so it can be selected and copied.
- Preserve native text selection for EPUB and text-based PDF content.
- If synchronization replaces or removes currently open content, keep the opened version readable for that session and notify the user. Apply the change on the next open rather than switching content underneath the reader.

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
- Saved Reading Progress synchronizes across devices. If an offline device reconnects with a different position after the server position changed, keep the server position and offer **Continue from this device's position**. Finished and Re-read actions synchronize separately and never erase completion history.

## Flex — Video Hub

### Ingestion and AV1 Processing

- Play movies, television shows, anime, and other video files.
- Rip optical discs from the headless server or a desktop client with an optical drive.
- Use `makemkvcon` and ffmpeg for disc ripping and AV1 encoding.
- Automatically encode uploaded or ripped video into AV1 for storage.
- Retain the original until the AV1 result is verified playable with every selected video, audio, subtitle, and chapter track.
- Before deleting the original, disclose every source video, audio, subtitle, or chapter track omitted from the AV1 result. Any omission requires explicit confirmation; otherwise retain the original.
- Until verification succeeds, the media remains unavailable for playback and incomplete output does not enter the library. Failure, cancellation, restart, or insufficient storage retains the original for retry.
- Do not re-encode an AV1 file when all included tracks are supported.
- Transcode AV1 for clients that cannot play it directly.

### Disc Identification and Track Selection

- For movies, identify the title using the disc volume label and compare candidate track durations with TMDB runtime data. Select the only confident match automatically.
- For television, automatically match standard episode tracks using the expected runtime window, disc order, volume label, and TMDB runtime data.
- Keep tracks outside the normal episode-duration range available for review so shorts, specials, double-length episodes, and unusual premieres are not discarded.
- Ambiguous movie and television mappings do not block ripping or encoding when every plausible content track is retained. Keep those tracks **Unmatched** for non-blocking batch review, and require confirmation only before excluding a plausible track.
- Filter menu tracks and obvious non-content tracks by minimum runtime, but retain them for manual review when uncertain.
- Group director's cuts, dubbed versions, extras, trailers, and alternate aspect ratios under one title without treating them as duplicates.

### Metadata and Organization

- Parse filenames with `torrent-name-parser`.
- Use AniList's GraphQL API for anime metadata and TMDB for movie and television metadata.
- Preserve existing thumbnails when available.
- Fetch name, date, rating, genre, description, cast, artwork, and television episode descriptions.
- Apply provider metadata and artwork automatically only when one candidate is uniquely confident. Low-confidence, tied, or conflicting matches remain **Unmatched** until the user confirms one.
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
- Required episodes are regular episodes in the selected provider ordering. Specials, extras, alternate cuts, and unmatched tracks are excluded unless the user includes them.
- Mark a season watched when all required episodes are watched or when the user marks it manually.
- Mark a series watched when all required available seasons are watched.
- New episodes return the series to in-progress.
- Metadata-only changes to episode order do not change watched status.
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
- A pre-change backup and previous server/mod configuration are protected from automatic cleanup and excluded from the five-backup limit until the user accepts the change or completes a rollback. After acceptance, the backup becomes unprotected and counts toward the limit.
- Create a daily backup only when world or player data changed since the last successful backup.
- Keep the latest five unprotected backups.
- User-protected backups do not count toward this limit. Selected restore-source and temporary safety backups are excluded only while restoration is active.
- During a running-server backup:
  1. Warn connected players that a backup is beginning.
  2. Temporarily pause world saving.
  3. Request a complete save and wait for confirmation.
  4. Establish a fixed backup source that cannot change after world saving resumes.
  5. Resume normal world saving.
  6. Write and verify the backup from that fixed source without keeping world saving paused.
- If save confirmation or copying fails, mark the backup unsuccessful, discard incomplete output, and always resume normal world saving.
- After any interrupted running-server backup, verify that world saving is enabled before normal play continues. If saving cannot resume, prominently warn the user and prevent silent continued play.
- When the server is stopped, back up the world directly without starting it.
- A successful backup is verified readable and records its date, world name, and Minecraft/Fabric configuration.
- If a backup is presented as able to restore its recorded configuration, retain everything required for that restoration. Otherwise, disclose at creation which versions or mods are not recoverable.
- Protected backups are excluded from automatic cleanup.
- Low, failing, full, or disconnected backup storage produces a clear warning and does not delete protected backups.

### Restoration

- List backups with their dates, versions, and status.
- Require the Minecraft server to be stopped before restoration.
- Immediately before restoration, revalidate the selected backup and show its world name, date, Minecraft version, and recorded configuration for confirmation. Failed validation aborts before the active world changes.
- Before replacement, create a safety backup unless the user explicitly declines.
- If the safety backup fails verification, do not begin restoration. Proceeding without it requires new explicit confirmation that no current-world recovery point exists.
- During restoration, temporarily protect the selected restore source and safety backup from cleanup.
- The five-backup limit may be exceeded until restoration succeeds or is canceled.
- A failed or interrupted restore leaves the existing world unchanged.
- After restoration, report which backup is active and compare its recorded Minecraft, Fabric, and mod configuration with the active configuration.
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

The system-wide External Service Availability rules apply to every row below, including automatic retry and preservation of completed local content. Rows list only provider-specific unavailable features, required input, or recovery exceptions.

| Service or source | Unavailable features | Recovery behavior |
|---|---|---|
| YouTube / YouTube Music | Music search, link matching, and new audio or playlist downloads | Reconnect an expired linked account when required; successful playlist tracks remain usable and failed entries remain unresolved. |
| Spotify | Connected-account search and Spotify-link resolution | Reconnect an expired linked account when required; failed imports remain unresolved. |
| LRCLIB | Lyrics and synchronized lyric timing | Import music without lyrics and retain existing or manually edited lyrics. |
| MangaDex and similar sources | Search, chapter lists, covers, metadata, and chapter downloads | Restart retrieval when the source cannot resume it; source removal does not remove downloaded chapters. |
| TMDB / AniList | Movie, television, and anime identification and metadata | Display unmatched media using its filename and existing local metadata; manual matches remain locked. |
| Mega and direct HTTP hosts | New and interrupted remote downloads | Request a renewed link, password, account access, decryption key, confirmation, or quota reset only when required. |
| BitTorrent metadata sources, trackers, and peers | Torrent metadata retrieval, peer discovery, and active downloads | Show waiting or availability status. Another tracker or magnet may continue the job only when it identifies the same content; otherwise, create a separate job. |
| Mojang/Minecraft, Fabric, Fabric API, and configured mod repositories | New installs, version checks, and updates | Keep the installed configuration unchanged and startable; never apply a partially retrieved update set. |
