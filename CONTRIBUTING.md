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

The `jumpstart-server` binary currently offers local initialization and offline inspection only. It does not start a daemon, pair devices, open a listener or manage a library.

On Linux or macOS, select a new child path of an existing scratch directory:

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development --json
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo --json
```

Initialization refuses every existing target, including an empty directory or symlink. It builds a private sibling staging directory, verifies the SQLite schema and authority identity, then publishes with an atomic no-replace rename. Failed staging is retained and identified for inspection. It is never adopted or removed automatically.

Offline inspection takes the existing shared maintenance lock and uses SQLite read-only immutable mode only after ruling out uncheckpointed WAL/rollback-journal data. It creates no files and performs no repair. Future writers must hold the exclusive maintenance lock for their connection lifetime. Active or interrupted writers require separate recovery work, not an inspection fallback.

The explicit `--development` flag acknowledges that encryption checks, volume registration, Linux filesystem qualification, sustained writing, backups, migrations and power-loss fault injection remain unimplemented. Storage functions return UnsupportedPlatform on Windows; portable domain/protocol tests still run there. Development smoke tests and the bundled SQLite tests do not establish production durability.
