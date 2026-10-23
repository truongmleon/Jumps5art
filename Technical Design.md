# Jumpstart Technical Design

## Document Status

- Status: Initial draft
- Source of truth for product behavior: `Requirements Doc.md`
- Purpose: Describe an implementable starting architecture and identify decisions that still require prototypes.
- Decision labels:
  - **Initial decision** — recommended baseline for implementation.
  - **Provisional** — use for prototyping, then confirm with evidence.
  - **Open** — requires a product, security, licensing, or performance decision.

This document does not replace the requirements. If the two documents conflict, the requirements take precedence until the conflict is resolved explicitly.

## 1. Design Goals

- Preserve user data through crashes, disconnects, full disks, failed updates, and interrupted migrations.
- Keep one authoritative server while supporting durable offline work on multiple devices.
- Minimize idle CPU, memory, storage, network, and GPU activity.
- Keep all private AI processing and indexes on the server.
- Isolate crash-prone and resource-heavy third-party tools from the main daemon.
- Prefer simple single-user operations over distributed-system complexity.
- Make incomplete, waiting, failed, and completed states unambiguous.

## 2. System Invariants

- Clients never modify server storage directly.
- Every logical server file has exactly one owning app.
- Canonical content becomes visible only after verification and an atomic metadata commit.
- Partial output never appears as a completed item.
- External provider failures never remove accepted local content or metadata.
- Destructive overwrite, merge, migration, or restore never occurs implicitly.
- Client wall clocks never decide synchronization order or conflict winners.
- A restart never changes an unknown operation into a successful one without reconciliation.
- Background processing yields to playback and interactive work.
- Canonical content, durable unsynchronized work, and protected recovery points are never cache-eviction candidates.

## 3. Architecture Overview

### 3.1 Server

**Initial decision:** implement `jumpstart-server` as one Tokio-based Rust modular monolith.

Logical modules:

- Transport and API
- Device authority
- Catalog, versions, and ownership
- Immutable content storage
- Synchronization and conflict resolution
- Durable jobs and resource scheduling
- Provider adapters
- Muse
- Photogenic
- Mass
- Bato
- Flex
- Worlds
- Backup, migration, and export
- Diagnostics and health

Modules communicate through typed Rust interfaces. Cross-module mutations go through application services and database transactions rather than directly changing another module's tables or files.

### 3.2 Worker Processes

Run crash-prone, untrusted, or resource-heavy operations outside the daemon:

- ffmpeg and ffprobe
- `makemkvcon`
- BitTorrent and remote-download engines
- document extraction and OCR
- face, semantic-search, and SDXL runtimes
- Minecraft Java process
- mod inspection and dependency tools

Workers receive job-specific staging directories and the minimum required inputs. They never publish directly into canonical storage.

### 3.3 Clients

Each Tauri client contains:

- UI frontend
- shared Rust client core
- local SQLite state
- encrypted device credentials
- local download and cache manager
- durable offline-operation queue

Mass desktop clients additionally use a platform-specific virtual-drive helper. Worlds is a thin authenticated CLI that sends commands to the daemon.

## 4. Persistence

### 4.1 Database

**Initial decision:** SQLite in WAL mode.

Configuration:

- foreign keys enabled;
- explicit schema versions;
- short write transactions;
- one database-writer boundary inside the daemon;
- SQLite online backup API for snapshots;
- integrity and foreign-key checks during verification.

SQLite matches the single-user, single-server workload and minimizes deployment and idle overhead. PostgreSQL is not part of the initial implementation. Reconsider it only if measured workload limits justify the additional operational complexity.

### 4.2 Server Storage Layout

```text
root/
  format.json
  database/
    jumpstart.sqlite
  objects/
    <hash-prefix>/<content-hash>
  staging/
    uploads/
    jobs/
  derived/
    previews/
    transcodes/
    indexes/
  worlds/
    active/
    recovery-catalog/
  diagnostics/
```

**Initial decision:** immutable content-addressed object storage.

- Each destination volume has staging on the same filesystem as its final object directory. Objects are written, hashed, validated, synchronized where required, and renamed into place before their database references are committed.
- Cross-filesystem placement uses copy, verification, and destination-local promotion rather than an atomic rename. Crash-created unreferenced objects are reconciled conservatively.
- Database records reference only promoted objects.
- Names, folders, app ownership, and organization live in the database rather than canonical filesystem paths.
- A logical item can contain multiple resources, such as the HEIC and MOV components of one Live Photo.
- Regenerable previews, transcodes, indexes, and client caches remain separate from canonical content.
- Crash-created unreferenced objects are reclaimed only after a conservative grace period.

**Provisional:** use BLAKE3 as the internal content hash. Confirm interoperability and recovery-tool requirements before freezing the storage format.

### 4.3 Identity

Recommended identities:

- `item_id`: UUIDv7 for one logical library item.
- `version_id`: one immutable accepted content version.
- `resource_id`: one component of a version.
- `blob_id`: immutable content hash.
- `device_id`: random stable identifier bound to a device public key.
- `operation_id`: device ID plus monotonic device sequence.
- `change_seq`: server-assigned synchronization cursor.

Names, paths, mount points, device labels, provider IDs, and stream indexes are not primary identities.

### 4.4 Ownership and Deduplication

- Ownership belongs to a logical item, not its physical object.
- Byte-identical logical items may reference the same immutable object.
- Deleting one item never removes an object still referenced by another live or recoverable item.
- A cross-app **Move** changes ownership only after destination validation succeeds.
- A cross-app **Copy** creates an independent destination-owned item, even if it initially references the same immutable bytes.
- Later edits use copy-on-write versions.

### 4.5 Lifecycle, History, and Retention

Store Activity History, unresolved conflicts, Recently Deleted entries, and Backup History as separate durable records.

- Activity History retains actions for seven days, identifies the originating device, and can reference a compensating Undo operation.
- Undo defaults to reversible actions from the requesting device. Another device's action must be selected explicitly from Activity History.
- Unresolved conflicts remain until resolved; their seven-day history period begins at resolution.
- Deletion moves recoverable item content and metadata to shared Recently Deleted for 30 days.
- Permanent deletion removes the item and its app metadata after all retention references are released.
- Restore name or location collisions require **Restore as Copy**, **Replace Existing**, or **Cancel**.
- Same-name uploads remain staged until **Replace**, **Keep Both**, or **Cancel** is committed; they never overwrite by name alone.
- Backup History is independent of Activity History and Recently Deleted and retains deleted or replaced backup versions for 30 days.
- An object is eligible for garbage collection only when no live item, Recently Deleted entry, unresolved conflict, protected recovery point, migration state, or retained backup references it.

## 5. API, Pairing, and Networking

### 5.1 Device Credentials

**Initial decision:**

- Generate an Ed25519 keypair on each client.
- Store the private key in the platform credential store.
- Pair on the same LAN with a short-lived one-time challenge.
- Approve from an existing device or a server-local command.
- Bind the issued device credential to its public key.
- Authenticate every connection and mutation.
- Retain revoked device IDs so revocation cannot be silently undone.

Device display names provide no authority and need not be unique.

### 5.2 Transport

**Provisional:** prototype iroh first.

Required transport capabilities:

- authenticated QUIC connections;
- LAN-direct path preference with bounded fallback;
- request/response RPC;
- change-event subscription;
- resumable bulk transfer;
- range-based media and virtual-drive reads;
- protocol and capability version negotiation;
- idempotency keys for mutations.

Before final selection, compare iroh and libp2p for LAN discovery, relays, NAT traversal, VPN behavior, mobile lifecycle restrictions, idle power, reconnect behavior, and resumable transfer.

### 5.3 Protocol Shape

- Version all request and event schemas.
- Return typed machine-readable errors plus short user-facing explanations.
- Distinguish unavailable, waiting, unsupported, invalid, failed, canceled, and not found.
- Stream large files independently from metadata RPC.
- Require a base entity revision for conflict-sensitive online mutations.

## 6. Synchronization and Conflict Resolution

Use an authoritative server change feed and durable client operation logs.

Each client mutation includes:

- operation ID;
- target identity;
- last observed entity revision;
- device-local monotonic sequence;
- operation payload and preconditions.

Server behavior:

1. Deduplicate the operation ID.
2. Preserve per-device offline order.
3. Validate the base revision.
4. Apply or create a conflict according to domain rules.
5. Assign a global `change_seq`.
6. Commit domain state and the outgoing change event in one transaction.

Conflict behavior:

- Merge independently identified additions, bookmarks, and annotations.
- Preserve incompatible content edits as separate versions or device-only copies.
- Represent deletion as a tombstone and Recently Deleted transition.
- Preserve losing edits in durable conflict records.
- Keep authoritative server progress when an offline checkpoint is stale, while retaining the device checkpoint as an explicit continuation option.
- Implement Undo as a compensating operation.

No accepted user change is silently discarded.

## 7. Durable Jobs and Resource Scheduling

### 7.1 Job State

All long-running operations share a durable job model:

`Queued → Running → Paused | Waiting for Source | Waiting for Input | Verifying → Completed`

Terminal alternatives:

- Failed
- Canceled

Each job records:

- type and owning module;
- initiating device;
- source and destination identities;
- idempotency key;
- durable checkpoint;
- retry classification and next attempt;
- required resource claims;
- worker attempts and logs;
- verified artifacts;
- dependencies and batch membership.

### 7.2 Restart Reconciliation

After a client, daemon, or machine restart:

1. Compare the stored job state with staging output and source identity.
2. Verify confirmed checkpoints.
3. Resume only when continuation is safe.
4. Otherwise retain the job and perform or require a clean restart.
5. Never infer completion from output-file presence alone.

Disconnecting the initiating device does not cancel accepted server-resident work. Jobs still dependent on that device enter **Waiting for Source** or fail safely.

### 7.3 Scheduler

Resource claims:

- CPU threads
- memory
- GPU or encoder slots
- disk read/write bandwidth
- network bandwidth
- temporary and destination storage
- exclusive devices such as optical drives

Priority:

1. Playback and interactive reads
2. User-blocking operations and interactive transfers
3. Safety and lifecycle operations
4. Accepted background transfers and required processing
5. Indexing, recognition, deduplication, and optional AI

Use bounded worker pools, reserve storage before large outputs, pause cooperative background jobs under pressure, and wake on events rather than frequent polling.

## 8. Muse

### 8.1 Ingestion

Model a playlist import as one parent job with independent child items. A failed track does not roll back successful tracks.

Spotify-to-YouTube Music matching stores:

- source identity;
- normalized title and artist;
- duration;
- version labels;
- candidate scores and evidence.

Auto-select only when one candidate exceeds an absolute confidence threshold and a margin over the next candidate. Persist the selected recording identity so retry never silently chooses a different recording.

Deduplication:

- exact object hash automatically reuses the existing track;
- acoustic fingerprint plus alignment detects likely re-encodes, silence, and intros;
- approximate matches always remain review proposals.

### 8.2 Playlists

Store playlists separately from entries. Entries reference tracks or unresolved placeholders and retain their source position.

For a provider-linked playlist, retain the last accepted provider snapshot. Updating performs a three-way comparison among:

- previous provider snapshot;
- current provider snapshot;
- current Jumpstart playlist.

Provider additions and reorders update source-origin entries. Preview removals. Preserve manual entries. This Device playlists remain entirely local.

### 8.3 Metadata

Every metadata field records value, provenance, confidence, provider revision, and manual-lock state. Automatic provider changes produce proposals when they conflict with accepted or manually edited data.

LRCLIB failure does not block ingestion or playback.

### 8.4 Playback and Handoff

Each player owns an independent playback session with a session generation, target device, queue, position, shuffle order, repeat mode, and monotonic command sequence.

Handoff uses prepare/commit:

1. Source keeps playing.
2. Destination validates availability and prepares the session.
3. Destination begins playback and reports its actual position.
4. Server increments the session generation and commits the destination as active.
5. Source stops only after commit.

Commands targeting an old session generation are discarded.

## 9. Photogenic

### 9.1 Camera Identity and Tombstones

Native library identifiers are observations, not canonical identity.

Store resource roles, sizes, filenames, capture metadata, native identifiers, verified resource hashes, and server linkage. Use a manifest of verified original resource hashes and roles as the durable unchanged-content identity.

Deleting a server item creates a tombstone keyed by that durable identity. An unchanged native asset remains suppressed after reinstall or device replacement. A changed resource manifest becomes a related version.

### 9.2 Compound Assets

Represent a photo as an aggregate with versions and typed resources:

- primary still;
- motion;
- RAW;
- JPEG;
- HDR gain map;
- depth;
- adjustment sidecar;
- edited rendition.

Compound assets become visible only after all required resources are verified and committed together.

### 9.3 Reversible Editing

Keep originals immutable. Store edits as versioned recipes and derivatives:

- crop and orientation parameters;
- markup layers;
- metadata overlays;
- still-only version created from a Live Photo;
- new video asset by default.

Revert changes the current-version pointer rather than reconstructing the source.

### 9.4 Face and Semantic Search

AI indexes are derived and rebuildable. Record model, model version, preprocessing version, and source content version with every embedding.

User corrections are authoritative constraints outside model output:

- must-link for verified merges;
- cannot-link for manual separations;
- named reference profiles;
- reversible correction events.

Publish replacement index generations atomically so search can continue using the previous complete generation during rebuild.

### 9.5 SDXL

Run SDXL in an isolated worker with:

- no network access;
- read-only model weights;
- job-specific input and output directories;
- CPU, memory, VRAM, and runtime limits;
- explicitly mounted source images only.

Validate generated output before ingesting it as a new AI-labeled asset. Store model, model version, seed, prompt, and source provenance locally. SDXL is the lowest-priority GPU workload and starts only when sufficient resources are available.

## 10. Mass

### 10.1 Remote Downloads

Provider adapters expose source probing, identity, resumability, range or piece retrieval, verification, and required user input.

- HTTP resume requires a strong immutable validator.
- Mega records node identity, key, size, and verification state.
- BitTorrent pins info hash, file selection, piece layout, and piece hashes.
- Login, quota, error, and interstitial pages are failures.

Partial bytes stay in job staging. If source identity cannot be proven unchanged, retain the job and require a clean restart.

### 10.2 VPN Enforcement

**Provisional technical design:** use a dedicated Linux network namespace for protected torrent workers.

- Establish the selected managed VPN inside the namespace.
- Provide no ordinary default route.
- Permit IPv4, IPv6, DNS, tracker, DHT, and peer traffic only through the tunnel.
- Communicate with the daemon through a local socket.
- Tunnel loss removes the only usable route before the daemon reports and pauses the job.

Prototype this design before choosing supported VPN types. Application-level interface checks alone are not an adequate kill switch.

### 10.3 Routing to Owning Apps

1. Keep the completed download Mass-owned.
2. Ask the destination app to validate the immutable input.
3. Run destination processing, such as Flex AV1 conversion.
4. Transfer ownership only after destination acceptance.
5. Leave ownership unchanged on failure.
6. Retain Mass job history as provenance.

Copy creates another logical item; Move reuses the stored bytes where safe.

### 10.4 Virtual Drive

Provisional platform adapters:

- Linux: FUSE3
- Windows: Cloud Files API or WinFsp after prototype
- macOS: File Provider extension with a native helper

Reads use a chunk cache. Open handles pin required chunks. Concurrent identical reads coalesce. Missing offline ranges return an unavailable error rather than zero-filled data.

Writable Mass files use durable shadow files and a write journal outside the evictable cache. Sync seals one immutable local version and commits it with a base-version precondition. If the server version changed, preserve both versions and create a conflict.

### 10.5 AI File Search

Use:

- Tantivy for filenames, metadata, and extracted text;
- a local vector index for semantic retrieval.

Extraction and embedding run in sandboxed workers with file-size, archive-expansion, memory, and time limits. Each indexed item records its content version, extractor/model version, and status: Pending, Indexed, Unsupported, or Failed.

No file content, extracted text, or embedding leaves the server.

## 11. Bato

### 11.1 Validation

Validate before publication:

- PDF structure, page tree, encryption, and representative decoding;
- EPUB container, package, manifest, spine, required resources, and unsupported DRM;
- CBZ archive safety, expansion limits, image decoding, and immutable page manifest.

Password input is ephemeral unless a later security decision defines secure password retention.

### 11.2 Versions and Anchors

Separate publication identity from immutable content versions.

Anchors reference a specific version:

- EPUB: CFI, spine identity, text quote/context, and content hash;
- PDF: page identity, normalized coordinates, optional text quote, and page hash;
- CBZ: page-manifest entry, image hash, and normalized coordinates.

Migrate anchors automatically only when the destination resolves uniquely. Preserve every ambiguous anchor against the old version as unresolved.

### 11.3 Page Ordering

Natural page ordering:

1. Normalize path separators and Unicode.
2. Split path components into text and numeric runs.
3. Compare numbers numerically.
4. Use archive order as the final tie-breaker.
5. Preserve every page and record a non-blocking ambiguity warning.

### 11.4 Prefetch and Cache

Create at most one prefetch intent for a device, current item, and reading-sequence revision. Deduplicate server ingestion across devices while allowing each reading device its own local cache copy.

Evict previews first, then closed and reproducible prefetches. Never automatically evict explicit downloads, open content, or pending writes. Promoting prefetch to explicit download changes cache classification without retransferring bytes.

### 11.5 Reading Progress

Store current viewing location, synchronized saved progress, and append-only completion history separately.

Progress operations carry the last observed server revision and content-version anchor. On stale progress, keep the server position and preserve the device position as **Continue from this device's position**. Finished and Re-read are separate revisioned events.

## 12. Flex

### 12.1 Ingestion Pipeline

1. Acquire into staging.
2. Probe every program, video, audio, subtitle, attachment, and chapter.
3. Create an explicit selected-track manifest.
4. Encode or remux into AV1-compatible canonical output.
5. Verify selected track mapping, timeline decoding, timestamps, seekability, chapters, subtitles, and duration.
6. Atomically promote output and metadata.
7. Delete the original only under the approved retention and omission rules.

Use Matroska as the provisional canonical container because it can retain PGS subtitles, attachments, multiple audio tracks, and chapters.

Track identity is independent of changing stream indexes. Preserve source identity, technical fingerprint, and stable logical identity, plus an explicit source-to-output mapping.

### 12.2 Disc Ripping

Acquire an exclusive optical-drive lease. A server drive uses a supervised `makemkvcon` worker. A desktop drive uses a signed helper and uploads completed title artifacts plus manifests.

A rip succeeds only when every selected title is read and verified. A playable but damaged or incomplete title remains failed.

Disc classification happens after preservation. Confident candidates apply automatically; ambiguous tracks remain playable and Unmatched.

### 12.3 Playback

Clients submit capability descriptions. Select:

- Direct play
- Remux
- Transcode

Use libmpv for playback and ffmpeg for remuxing/transcoding. Transcode sessions produce disposable HLS/fMP4 segments keyed by media version, capabilities, selected tracks, and quality.

Track preferences reference stable logical track identities and distinguish normal, commentary, descriptive, forced, signs-only, and full roles.

### 12.4 Progress

Each update includes base server revision, playback session, content revision, session sequence, position, and reason. A stale update never overwrites newer progress or watched state. Preserve it as an explicit device continuation candidate.

Seeking changes current location only. Durable progress advances after meaningful continued playback.

## 13. Worlds

### 13.1 Supervision

Represent Minecraft lifecycle as a persisted state machine:

- Absent
- Stopped
- Starting
- Running
- Stopping
- Crashed
- Recovery Required

Run Java in a dedicated process group or system scope. Persist process identity, configuration generation, and logs so the daemon can distinguish a surviving process from a reused PID after restart.

Use a serialized control lane for create, replace, restore, mod changes, start, stop, and restart.

### 13.2 Backups and Protection State

A running-server backup acquires save-control and backup leases, warns connected players, pauses saving, requests and confirms a complete flush, and establishes an immutable source. Prefer a filesystem snapshot or reflink. If unavailable, create a complete staging copy while saving is paused, subject to a safe timeout. Always resume and verify saving in a cleanup path before copying the immutable source to backup storage. Failure to establish the source fails the backup; failure to restore saving blocks continued play.

Create the daily backup only when world or player data changed since the previous successful backup. A missed scheduled opportunity produces at most one backup at the next eligible time and records the next attempt and last skip reason.

Retain the latest five unprotected backups. User-protected backups do not count toward this limit. Pre-change backups, prior configurations awaiting **Accept Change** or **Roll Back**, selected restore sources, and active safety backups remain protected until their workflow finishes.

Every successful backup records and verifies the world, Minecraft version, Fabric Loader, Fabric API, mod configuration, hashes, and artifact availability. If recorded software cannot be restored, disclose that limitation when creating the backup.

### 13.3 Restore

- Require the server to be stopped.
- Verify the source recovery point.
- Create and verify the safety backup unless explicitly declined.
- Materialize into staging on the same filesystem.
- Validate world files, checksums, and recorded configuration.
- Retain the current world as rollback state.
- Atomically activate the staged world.
- Keep source and safety protections until post-restore verification.

### 13.4 Mods

Maintain one immutable configuration generation and lockfile containing Minecraft, Fabric, API, mod versions, hashes, sources, dependencies, conflicts, and artifact availability.

Planning is side-effect free. Automatically add only uniquely resolvable required dependencies. Block unresolved dependencies and hard conflicts. Apply a complete staged generation only after artifact verification and a protected pre-change backup.

### 13.5 Unsafe Storage

If active world data cannot be saved reliably:

- block new player activity;
- attempt a clean save and stop;
- retain logs and storage diagnostics;
- require world or backup verification before reopening.

## 14. Backup, Migration, and Export

### 14.1 Backup Topology and Identity

One backup destination may protect multiple primary volumes. Each registered primary has a stable `primary_id`, a separate backup folder, and Backup History independent of its name or mount point.

**Provisional:** define the volume-fingerprint and replacement-drive protocol through a storage prototype. Missing, duplicated, or conflicting identities suspend synchronization until the user identifies the primary. An empty or replacement primary requires **Recover Existing Primary** or **Start as New Primary** before synchronization begins.

### 14.2 Consistent Backup Generations

1. Pin one committed database generation and its immutable object manifest.
2. Pin one consistent Worlds data and recovery-catalog generation.
3. Record primary identities, exclusions, and both generations in the backup job.
4. Copy missing immutable objects and Worlds data while later writes continue into newer generations.
5. Verify the database snapshot, manifests, Worlds generation, and included objects.
6. Publish the recovery point and completion marker atomically.

Only explicit daemon deletion events become backup deletions. Unexpected missing, unreadable, or corrupt primary data marks the primary degraded and cannot propagate into healthy backups. Removing a primary archives its backup for 30 days unless explicitly purged.

### 14.3 Restore, Migration, and Export

Before a full export or migration, query reachable devices for unsynchronized durable work and offer to synchronize or export it. Unavailable devices do not block the server snapshot, but the result records that their state could not be assessed.

Inventory an occupied destination and create and verify a destination recovery point before changing it. Restore or migrate into destination-local staging, verify content, metadata, schema, and declared exclusions, then activate only the complete generation. Never merge, overwrite, or delete destination data implicitly.

Keep the source unchanged until the user confirms successful recovery. Persistent schema or storage migrations require a verified pre-migration recovery point and durable phase tracking.

Export retained originals or documented common formats plus readable sidecars and a machine-readable manifest of exported, omitted, and failed items.

Provide server-local inspection and restoration tooling that does not require the primary database or an approved client.

## 15. Security and Privacy

- Keep provider secrets in a dedicated encrypted secret store.
- Redact tokens, credential-bearing URLs, file contents, prompts, locations, and face data from logs.
- Disable network access for local AI workers.
- Do not provide cloud AI fallback.
- Treat embeddings, face crops, prompts, and extracted text as sensitive derived data.
- Authenticate every client operation.
- Rate-limit pairing and one-time challenges.
- Validate archive paths and expansion limits.
- Run remote-download workers with restricted filesystem access.
- Treat VPN kill-switch behavior as a packet-level property verified by tests.

**Open:** define the at-rest encryption threat model and recovery-key experience before selecting encryption technology.

## 16. Observability and Recovery

- Structured logs with request, operation, job, device, and component correlation IDs.
- Local health for database, drives, storage reserve, scheduler, providers, jobs, and Minecraft.
- Bounded local metrics for latency, throughput, retries, queue depth, and resource pressure.
- Child-process logs attached to their jobs.
- User-exportable diagnostic bundles with explicit disclosure.
- Activity History remains a user-facing product record, not a view over debug logs.

Startup reconciliation checks running jobs, staging output, orphaned objects, worker processes, and pending event-outbox records before accepting new work.

## 17. Testing Strategy

### 17.1 Required Test Classes

- Unit tests for domain state machines and validation.
- Property tests for operation idempotency, ordering, and manifest round trips.
- Integration tests with real SQLite and filesystem boundaries.
- Worker-contract tests with malformed and partial output.
- Cross-device synchronization replay tests.
- Fault injection at every publish, backup, restore, and migration phase.
- Platform virtual-drive compatibility tests.
- Network leak tests for protected torrents.
- Recovery drills from verified backups without a primary database.

### 17.2 Critical Scenarios

- Kill the daemon during every ingestion stage.
- Fill or disconnect each storage volume during writes.
- Replay offline operations out of order.
- Revoke a device with unsynchronized work and active jobs.
- Restore into an occupied destination.
- Interrupt database and storage-format migration.
- Replace provider responses with malformed, empty, rate-limited, and destructive valid changes.
- Verify Live Photo and compound-asset atomicity.
- Exercise playlist source updates with manual additions and placeholders.
- Saturate CPU, GPU, disk, and network during playback.
- Kill the VPN during tracker, DHT, and peer traffic.
- Test virtual-drive atomic-save and rename-overwrite patterns.
- Replace ebook and chapter content while preserving anchors.
- Verify every selected disc track survives AV1 processing.
- Interrupt every Worlds backup and restore phase.
- Rebuild indexes without changing canonical content.

## 18. Delivery Plan

1. Foundation: database, object store, API, pairing, jobs, synchronization, and recovery.
2. Mass core: uploads, downloads, remote retrieval, ownership routing, and basic clients.
3. Backup and migration: verified snapshots, restore, export, and server-local recovery.
4. Muse: ingestion, playlists, playback sessions, and handoff.
5. Bato: validation, reader state, anchors, and prefetch.
6. Photogenic: camera backup, compound assets, edits, and duplicate review.
7. Flex: ingestion, AV1 verification, playback, and progress.
8. Worlds: lifecycle, backups, restore, and mods.
9. Local AI: file search, photo semantics, faces, and SDXL.
10. Hardening: fault injection, platform compatibility, performance limits, and migration drills.

Each stage requires successful restart and recovery tests before becoming the default path.

## 19. Open Decisions and Required Prototypes

### Networking

- iroh versus libp2p.
- Relay ownership and privacy.
- Mobile background and reconnect behavior.

### Storage

- Freeze BLAKE3 or another object-hash format.
- Define distribution of objects across multiple primary drives.
- Determine minimum supported Linux filesystems and snapshot capabilities.
- Decide backup encryption and recovery-key policy.

### Clients

- Virtual-drive adapters and supported filesystem semantics.
- Native mobile playback integration with Tauri.
- Background camera backup limits on each mobile platform.

### Media

- Acoustic fingerprint algorithm and safe duplicate thresholds.
- AV1 encoder and quality profiles.
- Canonical audio retention and codecs.
- HLS versus DASH for transcoded playback.
- Disc-tool licensing and distribution.

### AI

- Embedding and face models.
- Vector-index implementation.
- SDXL runtime isolation and supported hardware floor.
- Persisted extracted-text policy.

### Reader

- EPUB and PDF engines.
- Password-retention policy.
- Anchor-migration confidence thresholds.

### Worlds

- RCON, stdin, or plugin control channel.
- Mod repository trust and unavailable historical artifacts.
- Filesystem snapshot fallback and maximum save-pause duration.

### Prototypes

- LAN, NAT, relay, VPN, and mobile transport comparison.
- SQLite workload benchmark with jobs, sync, search, and virtual-drive reads.
- Cross-platform virtual-drive spike.
- VPN packet-leak test.
- Camera identity and deletion-tombstone recovery.
- Music matching and duplicate corpus.
- Playback handoff under failure.
- EPUB/PDF/CBZ anchor migration corpus.
- AV1 track-preservation and decode verification.
- Worlds backup/restore crash matrix.
- AI resource isolation under active playback.
