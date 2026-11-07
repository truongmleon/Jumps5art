# Development

Start with [why the Rust types exist](docs/Development/README.md) when reading these crates. It explains each type's purpose and the problem it addresses, with a worked `JobLifecycle` example and an index of the newer and earlier types. The guide links to the implementation in this checkout and distinguishes shared policies from the local snapshot runner.

Install [rustup](https://rustup.rs/). The compiler and tools are pinned in `rust-toolchain.toml`, and `Cargo.lock` pins dependencies.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`jumpstart-domain` implements validated identities, decimal-string counters, immutable content hashes and app ownership. `jumpstart-protocol` implements the version 1 handshake and bounded JSON framing. Neither opens a network listener. Wire fixtures live under `protocol/v1/`; neither crate claims the application RPC handlers or authenticated transport are implemented.

The protocol crate also checks half-open file read ranges and copies their raw
bytes from a supplied seekable source. `ReadRange::new` checks the actual resource
size and the 8 MiB per-range cap before I/O; zero length succeeds through EOF.
`copy_read_range` seeks to the accepted offset and streams exactly that length
through a 32 KiB buffer. A short source or failed write stays an error with any
partial destination retained. The caller owns authorization, immutable placement
pins, blob verification, cancellation/deadlines, and durable publication. These
are blocking library primitives; they do not expose a transfer service. See the
[range implementation](crates/jumpstart-protocol/src/range.rs) and
[raw-body/control-frame tests](crates/jumpstart-protocol/tests/ranges.rs).

The domain crate also checks job state transitions, fences stale worker results, and bounds automatic retries. For example, a canceled import cannot accept its old worker's late result, and an unavailable provider cannot trigger retries forever. These shared policies operate in memory. The storage crate now persists and executes the closed metadata-snapshot workflow using the lifecycle rules; general workers, automatic retry timers, and application publication remain future work.

CI runs the Rust checks on Linux, macOS and Windows. These library checks do not qualify the future Linux daemon, native applications or virtual-drive helpers for production.

PR descriptions identify implemented behavior, validation, remaining dependencies and any stacked base. Keep incomplete capabilities unavailable until the release gates in [Technical Design.md](<Technical Design.md>) pass.

## Metadata development preview

The `jumpstart-server` binary offers local initialization, offline inspection, verified metadata snapshots, recoverable metadata schema upgrades, filesystem capacity observations, and persisted local snapshot jobs. It does not start a daemon, pair devices, open a listener or manage a library.

On Linux or macOS, select a new child path of an existing scratch directory:

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development --json
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
```

Initialization refuses every existing target, including an empty directory or symlink. It builds a private sibling staging directory, verifies the SQLite schema and authority identity, then publishes with an atomic no-replace rename. Failed staging is retained and identified for inspection. It is never adopted or removed automatically.

Offline inspection takes the existing shared maintenance lock and uses SQLite read-only immutable mode only after ruling out uncheckpointed WAL/rollback-journal data. It creates no files and performs no repair. Future writers must hold the exclusive maintenance lock for their connection lifetime. Active or interrupted writers require separate recovery work, not an inspection fallback.

New initialization uses metadata schema 3. Existing schema 1 and 2 roots remain inspectable and can still be snapshotted; inspecting them never upgrades them automatically. Schema 2 introduced a stored change-sequence high-water mark. Schema 3 adds persisted snapshot requests and their versioned change records; their mutations advance that high-water mark. The network change-feed API and general durable scheduler remain unavailable.

The explicit `--development` flag acknowledges that encryption checks, volume registration, Linux filesystem qualification, sustained writing, complete server backups, restoration, whole-server migration and power-loss qualification remain unimplemented. Storage functions return UnsupportedPlatform on Windows; portable domain/protocol tests still run there. Development smoke tests and the bundled SQLite tests do not establish production durability.

## Local metadata snapshots

On Linux or macOS, create a snapshot from an initialized, offline metadata root into a **new** directory outside the source tree. The output's parent directory must already exist. Select fresh scratch paths if the examples have been run before:

```sh
cargo run -p jumpstart-server -- snapshot create --metadata-root /tmp/jumpstart-metadata-demo --output /tmp/jumpstart-metadata-snapshot-demo --development --json
cargo run -p jumpstart-server -- snapshot verify --snapshot-root /tmp/jumpstart-metadata-snapshot-demo --json
```

Creation reads the source under its shared maintenance lock and copies its SQLite bookkeeping through SQLite's Online Backup API. The source must be checkpointed, with no active exclusive writer or nonempty WAL/rollback journal. The operation verifies the copied database and manifest before publishing a separate bundle containing only `snapshot.json` and `metadata.sqlite`; an existing output is never replaced. Verification reads the bundle without changing it and does not require the original source or `--development`.

Reports identify schema 1 and 2 copies as `bootstrap_metadata`; schema 3 uses `metadata` because it also includes the persisted snapshot jobs and their change records. Reports include source server ID and synchronization epoch, metadata/schema versions, database size as a decimal string, and a tagged BLAKE3 digest. Schema 1 reports `change_seq` as `null`; schemas 2 and 3 capture the actual stored value as a decimal string. A new schema-3 root starts at `"0"`, then job mutations advance it. An unavailable value and a stored zero remain distinct.

This bundle captures local metadata, including the job records present in schema 3. It excludes media/object contents, Worlds data and backups, secrets and keys, logs, server settings, and device-only or unsynchronized work. It is not a restorable metadata root, a complete server backup, or an implemented whole-server recovery/migration workflow. The schema upgrade below retains a snapshot for inspection but does not implement restoring that copy. Keeping the snapshot on the source's physical disk does not protect either copy against that disk failing. The full backup requirements and independent-storage recovery gates in [Technical Design.md](<Technical Design.md>) remain separate work.

## Recoverable metadata schema upgrade

An older root should keep its server identity as its database layout evolves. On Linux or macOS, `migrate` upgrades a verified, offline schema 1 or 2 root to schema 3 while retaining a verified pre-upgrade snapshot. Select a **new** snapshot directory outside the source root, beneath an existing parent:

```sh
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
cargo run -p jumpstart-server -- migrate --metadata-root /tmp/jumpstart-metadata-demo --snapshot-output /tmp/jumpstart-before-schema-3 --development --json
```

The acknowledgment is checked before filesystem access. The upgrade takes the exclusive maintenance lock, verifies the old root, creates and verifies the snapshot, and records `migration.json` before changing the database. Each schema step commits its changes, migration checksum, and SQLite version in one database transaction. It then checkpoints and closes SQLite, synchronizes the database, replaces `format.json` atomically, verifies the completed root, and removes the journal with directory synchronization. The server ID and synchronization epoch are retained.

The journal is needed because a SQLite transaction and a separate `format.json` replacement cannot commit together. An interruption can leave the database upgraded while the marker still names the older schema. While the journal exists, ordinary inspection and snapshot creation refuse the root; only migration can reconcile that recorded workflow.

`MigrationReport` groups the result so a caller can tell what changed and locate the retained copy:

| Field | What it tells the caller |
|---|---|
| `metadata` | The verified metadata root and its identity after completion. |
| `from_schema_version`, `to_schema_version` | The starting and resulting database layouts. |
| `backup` | The verified pre-upgrade `SnapshotReport`, or `null` when no upgrade was needed. |
| `resumed` | Whether this call continued a previously recorded migration journal. |

A clean schema 3 root returns a successful no-op: both versions are 3, `backup` is `null`, and `resumed` is false. It creates no snapshot and leaves an existing snapshot-output path untouched. Supplying `--snapshot-output` is still required by the command so its invocation can also describe a real or resumed upgrade.

If an upgrade is interrupted after its journal has been recorded, retain the root, journal, and snapshot, then retry **the same command with the same paths**. Migration checks the recorded identities and retained snapshot before continuing; it does not silently select a different copy. Do not delete the journal, edit the marker, or remove SQLite sidecars to bypass a refusal.

There is one earlier interruption case: a snapshot may have been published before the journal was recorded. The source is still unchanged, and that snapshot remains intact. Because an existing output is never reused for a new migration, select another new snapshot-output path for that attempt. A completed migration with no remaining journal returns the current-schema no-op on later calls.

A journal left by the earlier format-1 migration still finishes its recorded 1-to-2 upgrade. To continue from that completed schema 2 root to schema 3, run `migrate` again with another new snapshot-output path. A retained recovery journal is never silently reinterpreted as a different upgrade.

This is a development upgrade for local metadata. It supplies recovery of its interrupted upgrade steps, not a full backup restore, whole-library migration, durable scheduler, or production filesystem qualification. Existing schema 1 and 2 snapshots remain verifiable without upgrading them.


## Persisted local snapshot jobs

A job keeps a request and its outcome after the command that accepted it exits. This preview supports one real operation: copying the metadata database through the existing snapshot implementation. Enqueuing saves the request; running it performs the copy and verification. These commands require schema 3, so explicitly migrate older roots first.

On Linux or macOS, choose a new output directory outside the metadata root:

```sh
cargo run -p jumpstart-server -- jobs enqueue-snapshot --metadata-root /tmp/jumpstart-metadata-demo --output /tmp/jumpstart-job-snapshot --development --json
cargo run -p jumpstart-server -- jobs list --metadata-root /tmp/jumpstart-metadata-demo --limit 50 --json
```

Use a state filter to find waiting requests even when newer completed or failed
requests fill the unfiltered page:

```sh
cargo run -p jumpstart-server -- jobs list --metadata-root /tmp/jumpstart-metadata-demo --state queued --limit 50 --json
```

Selection uses the recorded state before applying `--limit`, and matching jobs
remain ordered from newest to oldest acceptance position. Other accepted states
are `running`, `verifying`, `completed`, `failed`, and `canceled`. Filtered reports
include `state` alongside `jobs`; unfiltered reports keep their existing shape.
This read-only command needs no development acknowledgment. A live runner still
owns the exclusive maintenance lock, so listing does not offer concurrent live
monitoring.

Copy the returned `job_id` into the following commands:

```sh
cargo run -p jumpstart-server -- jobs inspect --metadata-root /tmp/jumpstart-metadata-demo --job-id JOB_ID --json
cargo run -p jumpstart-server -- jobs run --metadata-root /tmp/jumpstart-metadata-demo --job-id JOB_ID --development --json
```

Each enqueue creates a new request. If its reply is lost, list and inspect the saved jobs before enqueuing again. Command-receipt deduplication is not implemented yet. Duplicate queued requests cannot replace an existing snapshot; the later run can instead fail because the destination is occupied.

The saved `SnapshotJob` describes what the request is for and what happened:

| Field | Purpose |
|---|---|
| `job_id`, `output_path` | Identify this request and its selected destination independently of its current stage. |
| `state`, `revision`, `attempt_generation` | Describe the stage and retain the lifecycle's checks against outdated control requests and worker results. |
| `created_seq` | Identifies the change position at which the request was accepted. |
| `created_at_ms`, `updated_at_ms` | Supply UTC observation times for people; revisions and sequences decide update order. |
| `result` | The `SnapshotReport` verified and recorded at completion; current artifact health requires separate verification. |
| `error` | The retained explanation of a failed attempt. |

Job revisions, worker token counters, and change positions use decimal strings in JSON. Each accepted mutation updates the job, appends its full versioned change projection, and advances the stored high-water mark in one database transaction. This keeps a saved status from becoming disconnected from its change history.

`jobs run` executes synchronously while holding the exclusive maintenance lock. It commits lifecycle steps without keeping a SQLite transaction open across the bulk snapshot copy. Other controllers fail as busy while that command owns the root; this preview does not provide cancellation during a running copy. The command records completion only after the actual snapshot passes verification. If copying fails, it saves a `Failed` outcome, prints the job record, and exits unsuccessfully. A saved failure is not a successful snapshot.

If a successful `jobs run` reply is lost, repeat the same command with the same job ID. A saved `Completed` job returns its recorded `SnapshotJob` unchanged: no new copy, event, revision, worker token, or update timestamp. This reads a historical success; it does not verify the output again, find a moved artifact, or declare that the recorded path is healthy today. Use `snapshot verify --snapshot-root PATH` to check the artifact at its current location. Failed and canceled jobs remain closed. A process exit can leave uncheckpointed WAL even after completion commits; if the command reports that recovery is required, run `jobs recover` before repeating it. Recovery preserves committed completion and fences interrupted active work as failed.

To execute one waiting request without selecting its ID, use:

```sh
cargo run -p jumpstart-server -- jobs run-next --metadata-root /tmp/jumpstart-metadata-demo --development --json
```

`run-next` selects the oldest queued acceptance position and holds exclusive
maintenance access through execution. One call attempts at most one request,
including when that request fails. The JSON report contains `job`: a complete
saved outcome, or `null` when no queued work exists. An empty queue exits
successfully without adding a change; a saved `Failed` job is printed before a
nonzero exit. Terminal requests are skipped, and later queued requests wait for
another invocation. The command requires `--development` even for an empty queue.

Each invocation makes a fresh queue selection. After a lost reply or interrupted
command, inspect the saved jobs and explicitly recover when required before
selecting more work. Use `jobs run --job-id ID` to retrieve a known request's
recorded completion. Queue execution remains synchronous; polling, background
scheduling, automatic retries, and resource admission remain future work.

A still-queued request can be canceled using its inspected revision:

```sh
cargo run -p jumpstart-server -- jobs cancel --metadata-root /tmp/jumpstart-metadata-demo --job-id JOB_ID --expected-revision 1 --development --json
```

Use the revision currently shown by inspection. An outdated revision is refused, and terminal jobs cannot be reopened. Cancellation of queued work creates no snapshot and leaves an existing destination untouched.

After an interrupted runner exits, explicitly recover before inspecting its saved outcome. Ordinary read-only inspection can refuse an uncheckpointed WAL:

```sh
cargo run -p jumpstart-server -- jobs recover --metadata-root /tmp/jumpstart-metadata-demo --development --json
```

Recovery obtains exclusive access, fences interrupted active attempts, and records them as failed while retaining their artifacts. An existing output file does not prove the original job completed. Queued work remains queued. Create a new request with a new output path when retrying; this preview does not yet link retry requests, automatically restart old work, or delete partial output. If a storage failure prevents saving an outcome, the API returns an error and the job can remain active until explicit recovery succeeds.

List and inspect are read-only and need no acknowledgment. Enqueue, run, run-next, cancel, and recover check `--development` before opening storage. Lists are bounded to 1–100 records, defaulting to 50; list and recovery reports contain a `jobs` array. Other platforms return an explicit unsupported error. These are local administrative tools; authenticated app APIs, daemon admission, generic workers, automatic retries/timers, and full-server restore remain separate work.

### Read a job's history

`inspect` shows the latest status. To see the saved values that led to it, use the read-only history command:

```sh
cargo run -p jumpstart-server -- jobs history --metadata-root /tmp/jumpstart-metadata-demo --job-id JOB_ID
cargo run -p jumpstart-server -- jobs history --metadata-root /tmp/jumpstart-metadata-demo --job-id JOB_ID --json
```

Text mode shows each revision, state, and worker token. A successful run normally shows `queued → running → verifying → completed`: revisions `1, 2, 3, 4` and token counters `0, 1, 1, 2`. The token advances at completion to stop accepting that attempt's later reports; it is not a count of workers. Failed runs retain their recorded explanation. JSON includes the complete saved projections in order, rather than filling earlier entries with today's outcome.

History does not execute work, repair storage, or create a snapshot. It validates the ledger and returns at most 16 entries for this closed runner; unknown jobs and inconsistent records are explicit errors. It is a local per-job explanation, not a device synchronization feed. As with ordinary inspection, uncheckpointed WAL requires explicit job recovery first.

### Read pages of local job changes

History explains one request. `jobs changes` follows saved revisions across requests, using the global change positions committed with each job mutation. For example, a page can show one job being queued, a second being queued, and the first being canceled, without replacing those earlier projections with their latest status.

Read the root's current synchronization epoch and stored change position:

```sh
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
```

Then request changes after a position already held, supplying that epoch:

```sh
cargo run -p jumpstart-server -- jobs changes --metadata-root /tmp/jumpstart-metadata-demo --epoch EPOCH --after POSITION --limit 100 --json
```

The read-only command needs no development acknowledgment. Use the inspected `change_seq` to look for later local changes from that point. A fresh root permits `--after 0` to read all its retained changes. Keep the server identity, epoch, and position together; an epoch from another history is refused even when the numeric position exists.

Each page contains `server_id`, `sync_epoch`, the requested `after`, continuation `next`, captured `high_water`, `minimum`, and `batches`. Each batch carries its exact `seq`, `payload_version`, and saved `job` projection. Text mode shows a concise sequence/job/revision/state timeline and the same cursor bounds. JSON emits the complete response compactly on one line: its JSON body is bounded to 1 MiB, followed by a newline. The row limit is 1–500, defaulting to 100; the byte budget can return fewer rows.

Continue with the same epoch and the returned `next` as the next `--after`. `next < high_water` means that more pages remain at the captured high-water position. An empty page at that position is a valid result; an unknown epoch or a future position is an error.

`minimum` is the lowest usable `after` cursor, inclusive. Upgrading older metadata preserves its existing change position as the history floor rather than inventing earlier job events. A cursor below that floor requires a new verified metadata-snapshot baseline; blindly advancing an old cursor would omit state the caller never obtained. The command reads changes after an existing baseline and does not construct or apply that baseline itself.

This is local inspection of snapshot-job records. It does not acknowledge device cursors or provide an authenticated synchronization endpoint. Ordinary uncheckpointed WAL still requires explicit job recovery before reading.

## Storage capacity observations

On Linux or macOS, observe the filesystem containing an existing directory, for example before planning a large import. The positional path identifies an ordinary directory; it does not need to be a Jumpstart metadata root:

```sh
cargo run -p jumpstart-server -- storage inspect .
cargo run -p jumpstart-server -- storage inspect . --json
```

This read-only command needs no `--development` acknowledgment and creates no metadata. It refuses missing paths, regular files, and a final directory symlink. Other platforms, including Windows, return an explicit unsupported-platform error. The separate `inspect --metadata-root ...` command continues to verify offline metadata identity and format.

The text heading identifies a **Storage capacity observation (not disk health or write approval)**. JSON reports use `scope: "capacity_only"`, a canonical directory path, and decimal strings for `total_bytes`, `free_bytes`, and `available_bytes`. The nested assessment reports warning and safety-reserve thresholds, available bytes above the reserve, and whether this observation crosses either threshold.

The default warning threshold is 10% of total capacity, rounded up to a whole byte; for filesystems of at least 50 GiB it is also at least 5 GiB. The safety reserve is the smaller of 256 MiB and 1% of total capacity, rounded down. This reserve marks space that future write admission should leave for recording failures, shutting down safely, and preserving recovery state as ordinary imports fill a volume.

These values are a point-in-time filesystem observation and may change immediately. The assessment does not reserve space or enforce admission. It provides no SMART or disk-health diagnosis, encryption check, registered-volume identity verification, quota or permission approval, or assurance that a write will succeed. Those storage qualification and reservation requirements remain separate work.
