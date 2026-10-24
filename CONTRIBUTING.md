# Development

Install [rustup](https://rustup.rs/). The compiler and tools are pinned in `rust-toolchain.toml`, and `Cargo.lock` pins dependencies.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The first foundation crate, `jumpstart-domain`, implements validated identities, decimal-string counters, immutable content hashes and app ownership. It has no filesystem, UI or network effects. More crates join the workspace in independent implementation slices.

CI runs the Rust checks on Linux, macOS and Windows. These library checks do not qualify the future Linux daemon, native applications or virtual-drive helpers for production.

PR descriptions identify implemented behavior, validation, remaining dependencies and any stacked base. Keep incomplete capabilities unavailable until the release gates in [Technical Design.md](<Technical Design.md>) pass.
