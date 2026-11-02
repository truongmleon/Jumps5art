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

The `jumpstart-server` binary offers local initialization, offline metadata inspection, verified bootstrap metadata snapshots, and point-in-time filesystem capacity observations. [docs/CLI/README.md](docs/CLI/README.md) lists each command, its arguments, and the report it prints. The binary does not start a daemon, pair devices, open a listener, or manage a library.

On Linux or macOS, select a new child path of an existing scratch directory:

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development --json
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
```

Initialization refuses every existing target, including an empty directory or symlink. It builds a private sibling staging directory, verifies the SQLite schema and authority identity, then publishes with an atomic no-replace rename. Failed staging is retained and identified for inspection. It is never adopted or removed automatically.

Offline inspection takes the existing shared maintenance lock and uses SQLite read-only immutable mode only after ruling out uncheckpointed WAL/rollback-journal data. It creates no files and performs no repair. Future writers must hold the exclusive maintenance lock for their connection lifetime. Active or interrupted writers require separate recovery work, not an inspection fallback.

The explicit `--development` flag acknowledges that encryption checks, volume registration, Linux filesystem qualification, sustained writing, complete server backups, restoration, migrations and power-loss fault injection remain unimplemented. Storage functions return UnsupportedPlatform on Windows; portable domain/protocol tests still run there. Development smoke tests and the bundled SQLite tests do not establish production durability.

## Local bootstrap metadata snapshots

On Linux or macOS, create a snapshot from an initialized, offline metadata root into a **new** directory outside the source tree. The output's parent directory must already exist. Select fresh scratch paths if the examples have been run before:

```sh
cargo run -p jumpstart-server -- snapshot create --metadata-root /tmp/jumpstart-metadata-demo --output /tmp/jumpstart-metadata-snapshot-demo --development --json
cargo run -p jumpstart-server -- snapshot verify --snapshot-root /tmp/jumpstart-metadata-snapshot-demo --json
```

Creation reads the source under its shared maintenance lock and copies its SQLite bookkeeping through SQLite's Online Backup API. The source must be checkpointed, with no active exclusive writer or nonempty WAL/rollback journal. The operation verifies the copied database and manifest before publishing a separate bundle containing only `snapshot.json` and `metadata.sqlite`; an existing output is never replaced. Verification reads the bundle without changing it and does not require the original source or `--development`.

Reports explicitly identify the scope as `bootstrap_metadata`. They include the source server ID and synchronization epoch, metadata/schema versions, database size as a decimal string, and a tagged BLAKE3 digest. `change_seq` is `null` because schema 1 does not store a change-feed watermark; it is unavailable, rather than a fabricated sequence of zero.

This bundle captures the current bootstrap bookkeeping only. It excludes media/object contents, Worlds data and backups, secrets and keys, logs, server settings, and device-only or unsynchronized work. It is not a restorable metadata root, a complete server backup, or an implemented recovery/migration workflow. Keeping the snapshot on the source's physical disk does not protect either copy against that disk failing. The full backup requirements and independent-storage recovery gates in [Technical Design.md](<Technical Design.md>) remain separate work.

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
