# Development

Install [rustup](https://rustup.rs/). The compiler and tools are pinned in `rust-toolchain.toml`, and `Cargo.lock` pins dependencies.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`jumpstart-domain` implements validated identities, decimal-string counters, immutable content hashes and app ownership. `jumpstart-protocol` implements the version 1 handshake and bounded JSON framing. Neither opens a network listener. Wire fixtures live under `protocol/v1/`; neither crate claims the application RPC handlers or authenticated transport are implemented.

The domain crate also checks job state transitions, fences stale worker results, and bounds automatic retries. For example, a canceled import cannot accept its old worker's late result, and an unavailable provider cannot trigger retries forever. These are in-memory rules for a future scheduler; durable job records, worker execution, timers, and publication still need implementation.

CI runs the Rust checks on Linux, macOS and Windows. These library checks do not qualify the future Linux daemon, native applications or virtual-drive helpers for production.

PR descriptions identify implemented behavior, validation, remaining dependencies and any stacked base. Keep incomplete capabilities unavailable until the release gates in [Technical Design.md](<Technical Design.md>) pass.

## Metadata development preview

The `jumpstart-server` binary offers local initialization, offline inspection, verified bootstrap metadata snapshots, and a recoverable metadata schema upgrade. It does not start a daemon, pair devices, open a listener or manage a library.

On Linux or macOS, select a new child path of an existing scratch directory:

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development --json
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
```

Initialization refuses every existing target, including an empty directory or symlink. It builds a private sibling staging directory, verifies the SQLite schema and authority identity, then publishes with an atomic no-replace rename. Failed staging is retained and identified for inspection. It is never adopted or removed automatically.

Offline inspection takes the existing shared maintenance lock and uses SQLite read-only immutable mode only after ruling out uncheckpointed WAL/rollback-journal data. It creates no files and performs no repair. Future writers must hold the exclusive maintenance lock for their connection lifetime. Active or interrupted writers require separate recovery work, not an inspection fallback.

New initialization uses metadata schema 2. Existing schema 1 roots remain inspectable and can still be snapshotted; inspecting them never upgrades them automatically. Schema 2 adds a stored change-sequence high-water mark, initially zero, for future sequenced library changes. It does not add a change feed or durable job execution.

The explicit `--development` flag acknowledges that encryption checks, volume registration, Linux filesystem qualification, sustained writing, complete server backups, restoration, whole-server migration and power-loss qualification remain unimplemented. Storage functions return UnsupportedPlatform on Windows; portable domain/protocol tests still run there. Development smoke tests and the bundled SQLite tests do not establish production durability.

## Local bootstrap metadata snapshots

On Linux or macOS, create a snapshot from an initialized, offline metadata root into a **new** directory outside the source tree. The output's parent directory must already exist. Select fresh scratch paths if the examples have been run before:

```sh
cargo run -p jumpstart-server -- snapshot create --metadata-root /tmp/jumpstart-metadata-demo --output /tmp/jumpstart-metadata-snapshot-demo --development --json
cargo run -p jumpstart-server -- snapshot verify --snapshot-root /tmp/jumpstart-metadata-snapshot-demo --json
```

Creation reads the source under its shared maintenance lock and copies its SQLite bookkeeping through SQLite's Online Backup API. The source must be checkpointed, with no active exclusive writer or nonempty WAL/rollback journal. The operation verifies the copied database and manifest before publishing a separate bundle containing only `snapshot.json` and `metadata.sqlite`; an existing output is never replaced. Verification reads the bundle without changing it and does not require the original source or `--development`.

Reports explicitly identify the scope as `bootstrap_metadata`. They include the source server ID and synchronization epoch, metadata/schema versions, database size as a decimal string, and a tagged BLAKE3 digest. For schema 1, `change_seq` is `null` because that schema stores no change-feed watermark. For schema 2, it is the captured stored value, initially the decimal string `"0"` because no sequenced library changes have occurred. An unavailable value and an actual stored zero remain distinct.

This bundle captures the current bootstrap bookkeeping only. It excludes media/object contents, Worlds data and backups, secrets and keys, logs, server settings, and device-only or unsynchronized work. It is not a restorable metadata root, a complete server backup, or an implemented whole-server recovery/migration workflow. The schema upgrade below retains a snapshot for inspection but does not implement restoring that copy. Keeping the snapshot on the source's physical disk does not protect either copy against that disk failing. The full backup requirements and independent-storage recovery gates in [Technical Design.md](<Technical Design.md>) remain separate work.

## Recoverable metadata schema upgrade

An older root should keep its server identity as its database layout evolves. On Linux or macOS, `migrate` upgrades a verified, offline schema 1 root to schema 2 while retaining a verified pre-upgrade snapshot. Select a **new** snapshot directory outside the source root, beneath an existing parent:

```sh
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
cargo run -p jumpstart-server -- migrate --metadata-root /tmp/jumpstart-metadata-demo --snapshot-output /tmp/jumpstart-before-schema-2 --development --json
```

The acknowledgment is checked before filesystem access. The upgrade takes the exclusive maintenance lock, verifies the old root, creates and verifies the snapshot, and records `migration.json` before changing the database. It applies the new schema, migration checksum, and SQLite version in one database transaction. It then checkpoints and closes SQLite, synchronizes the database, replaces `format.json` atomically, verifies the completed root, and removes the journal with directory synchronization. The server ID and synchronization epoch are retained.

The journal is needed because a SQLite transaction and a separate `format.json` replacement cannot commit together. An interruption can leave the database upgraded while the marker still names the older schema. While the journal exists, ordinary inspection and snapshot creation refuse the root; only migration can reconcile that recorded workflow.

`MigrationReport` groups the result so a caller can tell what changed and locate the retained copy:

| Field | What it tells the caller |
|---|---|
| `metadata` | The verified metadata root and its identity after completion. |
| `from_schema_version`, `to_schema_version` | The starting and resulting database layouts. |
| `backup` | The verified pre-upgrade `SnapshotReport`, or `null` when no upgrade was needed. |
| `resumed` | Whether this call continued a previously recorded migration journal. |

A clean schema 2 root returns a successful no-op: both versions are 2, `backup` is `null`, and `resumed` is false. It creates no snapshot and leaves an existing snapshot-output path untouched. Supplying `--snapshot-output` is still required by the command so its invocation can also describe a real or resumed upgrade.

If an upgrade is interrupted after its journal has been recorded, retain the root, journal, and snapshot, then retry **the same command with the same paths**. Migration checks the recorded identities and retained snapshot before continuing; it does not silently select a different copy. Do not delete the journal, edit the marker, or remove SQLite sidecars to bypass a refusal.

There is one earlier interruption case: a snapshot may have been published before the journal was recorded. The schema 1 source is still unchanged, and that snapshot remains intact. Because an existing output is never reused for a new migration, select another new snapshot-output path for that attempt. A completed migration with no remaining journal simply returns the schema 2 no-op on later calls.

This is a development upgrade for bootstrap metadata only. It supplies recovery of its interrupted upgrade steps, not a full backup restore, whole-library migration, durable scheduler, or production filesystem qualification. Existing schema 1 snapshots remain verifiable without upgrading them.
