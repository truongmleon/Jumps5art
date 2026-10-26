# Jumpstart

A self-hosted, single-user suite for music, photos, files, reading, video, and a Fabric Minecraft server.

The product contracts are in [Requirements Doc.md](<Requirements Doc.md>) and [Technical Design.md](<Technical Design.md>). Development is starting with the shared Rust foundation; the applications and remote server transport are not implemented yet.

## Development

Install [rustup](https://rustup.rs/). The repository pins the compiler and tooling in `rust-toolchain.toml`; Cargo pins dependencies in the committed `Cargo.lock`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The foundation crates are portable Rust libraries. CI checks Linux, macOS, and Windows. Passing these library checks does not qualify a platform for the future daemon, native apps, or virtual-drive helpers.

## Current implementation

| Crate | Scope |
|---|---|
| `jumpstart-domain` | Validated identities, counters, immutable content hashes and ownership |
| `jumpstart-protocol` | Version 1 handshake/types and bounded JSON framing; no network listener |

Changes are delivered in small PRs with their scope, tests and remaining dependencies recorded in each description. A dependent PR identifies its base explicitly so it can be reviewed separately.
