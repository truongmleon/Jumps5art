# F1 Foundation: trusted and recoverable library operations

## Purpose and source

One person uses Jumpstart from several devices. Before importing media or editing a library, they need to know that only their approved installations can reach it, that an interrupted command cannot silently run twice, and that accepted work survives a restart with an accurate status.

This PRD breaks down **System-Wide Foundation and Constraints** in [Requirements Doc.md](../../Requirements%20Doc.md) and the F1 stage in [Technical Design.md](../../Technical%20Design.md). It adds acceptance scenarios for review without changing the product behavior or selected architecture. The identifiers below belong to this checklist.

## Scope

F1 provides the shared storage, authority, framed protocol, synchronization, durable-job, and health foundations. The server remains single-user: every approved installation belongs to the same person. Device identity and revocation do not introduce accounts, roles, or sharing.

Five separately packaged apps use the shared foundation. The Linux daemon owns server mutations; clients and Worlds submit authorized requests rather than editing server storage directly. Platform work must cover macOS, Windows, Linux, iOS, iPadOS, and Android as required by the release gates. Portable Rust tests alone do not establish that coverage.

Media acquisition, app-specific playback/reading/editing, full backup/export generations, Worlds supervision, optional AI, and advanced native adapters retain their later delivery stages. F1 must preserve their required interfaces and invariants without advertising those features as available.

## Acceptance scenarios

| ID | Trigger and required observable result | Evidence needed | Contract |
|---|---|---|---|
| F1-01 Safe setup and updates | Setup identifies existing data before any approved adoption. Updates preserve a verified recoverable prior state; an interruption leaves the old state usable or explicitly recoverable. No diagnostic read initializes missing storage. | Existing-file/symlink fixtures, concurrent setup, occupied recovery destinations, and interruption tests across every publication boundary. | Requirements: App and Server Management; design §§4.1, 16.1–16.2. |
| F1-02 Trusted pairing | A new installation pairs only through an explicit same-LAN approval. The approval shows the bound key/phrase and enough app/platform context to distinguish same-named installations. Server-local approval works without an approved client. | First-device bootstrap; rejected, expired, replayed, substituted-key, forged-LAN, and rate-limited requests; restart invalidates pending approvals. | Requirements: Device Pairing; design §5.2. |
| F1-03 Immediate removal | Removal revokes access and terminates active connections/streams. Unaccepted mutations fail. Accepted server-resident work continues; work needing that device waits or fails safely. Local files and pending changes remain exportable. | Concurrent mutation/revocation and live upload/read/subscription tests, including the exact commit boundary and a previously revoked key attempting to pair again. | Requirements: Device Pairing, Long-Running Operations; design §§5.2, 7.2. |
| F1-04 Bounded communication and routes | A stale local route does not stall connection indefinitely; configured fallback routes remain available. Incompatible or malformed messages fail without accepting an operation or discarding pending work. | Direct-LAN-first/fallback traces, deadlines, transport compatibility, malformed and oversized frames, duplicate keys, unsupported versions/features, and bounded backpressure. | Requirements: Connectivity and Offline Use, Server-Client Networking; design §§5.1, 5.3–5.5. |
| F1-05 Unknown outcomes and replay | An offline action remains Pending Sync. A lost online reply becomes Awaiting Confirmation. Repeating the same operation returns its recorded outcome once; a changed payload under that identity is refused. Missing sequences are not guessed. | Durable local outbox/reconciliation tests; restart before/after commit/reply; duplicate/reordered requests; payload mismatch; receipt acknowledgment and compaction. | Requirements: Offline Conflict Resolution; design §6.1. |
| F1-06 Ordered catch-up and recovery | Clients apply a whole authoritative batch and its cursor together. Wrong-epoch or unavailable history requires explicit reconciliation, preserving pending edits and downloads. An incomplete snapshot never replaces the current cache. | Atomic cache/cursor crash tests, duplicate application, stale-cursor refresh, interrupted snapshot import, scope changes, and rollback-authority recovery. | Requirements: Offline Conflict Resolution, Migration and Export; design §§6.2–6.3. |
| F1-07 Conflicts and lifecycle | Compatible additions combine. Incompatible edits preserve both versions. Deletion wins for the server item while offline edited bytes remain recoverable. Same-name replacement/restore is an explicit choice. Undo respects originating-device rules. | Concurrent stale edit/delete/restore/undo fixtures, 7-day Activity History and 30-day Recently Deleted boundaries, unresolved-conflict retention, and stale resurrection prevention. | Requirements: Offline Conflict Resolution, History and File Lifecycle; design §§4.5–4.6, 6.4. |
| F1-08 Durable work | Restart preserves confirmed progress and reconciles actual sources/destinations. Cancellation fences late publication and removes incomplete output only after the worker stops, unless resumable data was retained. Completed children remain usable. | Process-exit and restart matrix; late-worker results; pause/resume and changed sources; partial batches; output verification; linked successor retries and bounded automatic waits. | Requirements: Long-Running Operations, External Service Availability; design §§7.1–7.3. |
| F1-09 Capacity and isolation | Low capacity warns; a full destination pauses affected producers and preserves confirmed data. Unaffected libraries, diagnostics, deletion, and recovery remain usable. Background work yields to interactive work. | Reservation/admission races, ENOSPC and missing-mount injection, separate capacity accounting, threshold boundaries, and measured playback/interactive isolation. | Requirements: Storage Capacity, Connectivity and Offline Use; design §§7.3, 14.3, 16.4. |
| F1-10 Actionable health | A component failure identifies affected data, current degradation, and a next action. Missing health evidence remains Unknown. Provider outages or invalid responses preserve earlier accepted content and metadata. | Component fault injection, bounded/redacted diagnostics, provider-response fixtures, preserved prior state, and notifications only for required input or exhausted retry budgets. | Requirements: External Service Availability, Storage Capacity; design §§7.2, 16.3–16.4. |
| F1-11 Usable platform states | Setup, reconnect, offline availability, pending work, and errors are understandable on every supported client target. Keyboard navigation where applicable, screen-reader labels, text scaling, contrast, reduced motion, and accessible controls work. | Native-device/platform scenarios and accessibility review; suspended or terminated mobile clients retain durable work and resume only in OS-granted opportunities. | Requirements: Client Platforms, Connectivity and Offline Use, Accessibility; design §§3.3, 17.3. |

The scenarios above are acceptance requirements, not claims of passing tests. Timing, resource, retention, and protocol limits remain those in the technical design; this checklist does not redefine them.

## What the current checkout proves

This assessment includes the durable snapshot-job/change-page PR stack through #36. It describes local development capability, not a released F1 daemon or client.

| Area | Present evidence | Remaining gap |
|---|---|---|
| Local metadata setup and upgrades | Checked initialization/inspection, no-replace publication, verified metadata snapshots, and explicitly recoverable schema upgrades. See [metadata](../../crates/jumpstart-storage/tests/metadata.rs), [snapshots](../../crates/jumpstart-storage/tests/snapshots.rs), and [migration tests](../../crates/jumpstart-storage/tests/migrations.rs). | Registered volumes, full content publication, complete backup/restore, Linux filesystem qualification, and the broader power-loss matrix. |
| Message compatibility | Bounded JSON framing and Hello/Welcome negotiation. See [framing](../../crates/jumpstart-protocol/tests/framing.rs) and [handshake tests](../../crates/jumpstart-protocol/tests/handshake.rs). | Authenticated transport, pairing, device authority, revocation, routing, and application RPC handlers. |
| Job rules and execution | Lifecycle fencing, bounded retry-policy decisions, and one persisted synchronous metadata-snapshot operation. See [lifecycle](../../crates/jumpstart-domain/tests/job_lifecycle.rs), [retry](../../crates/jumpstart-domain/tests/retry_policy.rs), and [saved-job tests](../../crates/jumpstart-storage/tests/jobs.rs). | General workers/scheduler, durable timers, active cancellation, pause/resume, cleanup, resource admission, and linked retries. The local runner retains interrupted artifacts for diagnosis. |
| Reading changes | Original local job projections, bounded pages, epoch checking, and a preserved migration floor. See [change-page tests](../../crates/jumpstart-storage/tests/job_changes.rs). | Authenticated general feed, client cache/outbox, receipt processing, acknowledgments, snapshots for client resync, compaction, and conflict application. Returning a completed job again does not deduplicate enqueue requests or implement operation receipts. |
| Capacity | Read-only filesystem observations and warning/reserve calculations. See [capacity tests](../../crates/jumpstart-storage/tests/capacity.rs). | Enforced reservations/admission, category accounting, ENOSPC pause/resume, registered identity, device health, and pressure isolation. |
| User-facing clients | Requirements and navigation contracts. | Native apps, their credential adapters, offline caches, accessibility evidence, and six-platform acceptance. |

## Review and implementation order

Use small implementation PRs to close the gaps while preserving the existing checked paths:

1. Establish device authority, protected server-local control, pairing, and authenticated request boundaries before exposing local diagnostic data over a network.
2. Add receipt processing and the durable client outbox/cache contract before claiming that retried app mutations have exactly-once effects.
3. Connect the general feed, snapshot refresh, conflicts, deletion, and Undo with transactional application tests.
4. Extend the job runner with worker reconciliation, cleanup, reservations, scheduling, and actionable health before enabling app jobs.
5. Attach platform, accessibility, performance, and recovery evidence before declaring F1 complete.

These are dependency checkpoints for the existing F1 contract. A focused helper can be built earlier, but its presence does not satisfy the entire user-visible scenario. In particular, local full database verification, a metadata-only snapshot, or a passing capacity calculation cannot stand in for authenticated sync, full recovery, or write admission.

## Completion evidence

Each acceptance row needs a linked implementation, its failure/restart fixtures, observed user-visible states, and any platform limits. The design's Foundation gates also require a first clean-host recovery drill, publication fault matrix, duplicate/revocation/replay coverage, and measured idle/resource behavior. Full backup/export and app-specific scope retain their later-stage gates; a metadata snapshot must continue to name its limited contents.

F1 remains incomplete while the gaps above are open. A PR can finish its bounded slice and still leave its parent acceptance scenario pending.
