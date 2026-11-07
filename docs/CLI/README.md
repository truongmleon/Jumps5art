# jumpstart-server commands

`jumpstart-server` is the local storage tool in this repository. From the repository root:

```sh
cargo run -p jumpstart-server -- --help
```

These are the commands the binary accepts. [CONTRIBUTING.md](../../CONTRIBUTING.md) describes the same preview in more detail, including how initialization and snapshots are published. The finished server's administrative commands are specified separately in [Technical Design.md](<../../Technical Design.md>) and are absent from `--help`.

The commands run on Linux and macOS. On Windows they exit with an error that storage operations are supported only on Linux and macOS, and they do not create or change files.

## Available commands

| Command | What it does |
|---|---|
| `init` | Creates a new metadata folder that identifies one server. |
| `inspect` | Reads an existing metadata folder and prints that identity. The folder stays unchanged. |
| `snapshot create` | Copies that folder's bookkeeping database into a new checked bundle. |
| `snapshot verify` | Checks an existing bundle. The original metadata folder is not required. |
| `migrate` | Upgrades older metadata after saving a verified recovery copy; see [migration instructions](../../CONTRIBUTING.md#recoverable-metadata-upgrades). |
| `storage inspect` | Reports how much space the filesystem has for an existing directory. |

`init`, `migrate`, and `snapshot create` require `--development`. The other commands do not.

A successful command prints its report on stdout and exits successfully. A failure prints `error:` and the reason on stderr, leaves stdout empty, and exits with a failure status. Add `--json` for pretty-printed JSON. Without `--json`, the first line is a heading and the fields follow, indented two spaces per level.

`--help` and `--version` are handled before a command runs. `snapshot --help` and `storage --help` list the nested commands.

## `init`

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development
```

| Flag | Required | Meaning |
|---|---|---|
| `--metadata-root` | yes | New directory. Its parent must already exist. |
| `--development` | yes | Acknowledges that this is a development preview. Production volume checks, encryption, backup, and restore are still unimplemented. |
| `--json` | no | Print the report as JSON. |

The new folder receives `format.json`, `metadata.lock`, and `database/jumpstart.sqlite`. The database records the server ID, the sync epoch, the format version, and the applied schema migration. Those identity values are generated once for this folder.

The path must be new. An existing directory, including an empty one, and an existing symlink are left unchanged and the command fails. Preparation happens in a private staging directory beside the destination. The folder is renamed into place only after the database and `format.json` agree. If preparation fails, that staging directory is kept and named in the error.

Heading: `Verified offline development metadata:`

| Field | JSON value |
|---|---|
| `root` | Absolute path of the published folder |
| `server_id` | UUID string for this server |
| `sync_epoch` | UUID string recorded with the server ID |
| `format_version` | Number, currently `1` |
| `schema_version` | Number, currently `3` |
| `change_seq` | Stored decimal-string position; unavailable (`null`) in schema 1 |
| `journal_mode` | SQLite journal mode of the stored database |
| `integrity_check` | `"ok"` after the check succeeds |

## `inspect`

```sh
cargo run -p jumpstart-server -- inspect --metadata-root /tmp/jumpstart-metadata-demo
```

| Flag | Required | Meaning |
|---|---|---|
| `--metadata-root` | yes | Existing metadata folder created by `init`. |
| `--json` | no | Print the report as JSON. |

This prints the same report as `init` for a folder that is already checkpointed and has no exclusive writer. A missing folder is not created. A nonempty WAL or rollback journal needs recovery first; this command does not repair it.

Heading: `Verified offline development metadata:`

## `snapshot create`

```sh
cargo run -p jumpstart-server -- snapshot create --metadata-root /tmp/jumpstart-metadata-demo --output /tmp/jumpstart-metadata-snapshot-demo --development
```

| Flag | Required | Meaning |
|---|---|---|
| `--metadata-root` | yes | Existing offline, checkpointed metadata folder to read. |
| `--output` | yes | New directory outside that folder. Its parent must already exist. |
| `--development` | yes | Acknowledges the limited metadata snapshot scope. |
| `--json` | no | Print the report as JSON. |

The command copies `database/jumpstart.sqlite` with SQLite's backup API, stores the copy as a single database file, and writes `snapshot.json` beside it. The manifest records the server ID, sync epoch, schema versions, database size, and a BLAKE3 digest of the database bytes. The bundle is published only after that copy checks out. An existing output directory is left unchanged.

The published directory contains only `snapshot.json` and `metadata.sqlite`.

`change_seq` is JSON `null`. Schema 1 has no change feed, so there is no watermark to record. The field is still present. A manifest that omits it, or that uses `0`, fails verification.

Heading: `Verified local metadata snapshot (not a full backup):`

| Field | JSON value |
|---|---|
| `root` | Absolute path of the published bundle |
| `snapshot_format_version` | Number, currently `1` |
| `scope` | `"bootstrap_metadata"` for schemas 1/2, `"metadata"` for schema 3 |
| `server_id` | Server UUID copied from the source |
| `sync_epoch` | Sync-epoch UUID copied from the source |
| `format_version` | Source metadata format, currently `1` |
| `schema_version` | Source schema, currently `1` |
| `change_seq` | `null` |
| `database_bytes` | Decimal string, the exact length of `metadata.sqlite` |
| `database_hash` | `b3:` followed by 64 lowercase hex digits |

`bootstrap_metadata` covers initial server bookkeeping in schemas 1/2. Schema 3 uses `metadata` because its database also includes saved snapshot jobs and their changes. Neither scope contains media, Worlds data, secrets, or logs. There is no command here that turns the bundle back into a metadata folder or a running server. Keeping the bundle on the same disk as the source does not protect either copy if that disk fails.

## `snapshot verify`

```sh
cargo run -p jumpstart-server -- snapshot verify --snapshot-root /tmp/jumpstart-metadata-snapshot-demo
```

| Flag | Required | Meaning |
|---|---|---|
| `--snapshot-root` | yes | Directory produced by `snapshot create`. |
| `--json` | no | Print the report as JSON. |

Verification reads `snapshot.json` and `metadata.sqlite` and prints the same report as `snapshot create`. It still succeeds after the original metadata folder is deleted. It does not modify the bundle. A missing bundle is not created. A damaged or extra file fails the command and is left as it was.

Heading: `Verified local metadata snapshot (not a full backup):`

## `storage inspect`

```sh
cargo run -p jumpstart-server -- storage inspect /tmp
cargo run -p jumpstart-server -- storage inspect /tmp --json
```

| Argument | Required | Meaning |
|---|---|---|
| path | yes | Existing directory. It does not have to be a metadata folder. |
| `--json` | no | Print the report as JSON. |

The path is the filesystem location to measure. A missing path, a regular file, or a path whose last component is a symlink fails, and those paths stay as they were. A symlink in a parent directory is allowed. `path` in the report is the canonical directory.

The command reads one point-in-time space report and calculates the default warning and reserve thresholds. It does not create metadata, reserve bytes, or decide that a later write will succeed.

Heading: `Storage capacity observation (not disk health or write approval):`

| Field | JSON value |
|---|---|
| `path` | Canonical absolute directory path |
| `scope` | `"capacity_only"` |
| `total_bytes` | Decimal string. Total size of the filesystem. |
| `free_bytes` | Decimal string. Free bytes, including space ordinary users cannot use. |
| `available_bytes` | Decimal string. Bytes the filesystem offers ordinary users. |
| `assessment` | Thresholds derived from this observation. |

`available_bytes` is at most `free_bytes`, and `free_bytes` is at most `total_bytes`. The assessment uses available bytes.

| Assessment field | Meaning |
|---|---|
| `warning_below_bytes` | Ten percent of `total_bytes`, rounded up to a whole byte. On a filesystem of at least 50 GiB, this is also at least 5 GiB. |
| `safety_reserve_bytes` | One percent of `total_bytes`, rounded down, and at most 256 MiB. A very small filesystem can report `0`. |
| `available_above_reserve_bytes` | Available bytes minus the reserve, with a minimum of `0`. |
| `below_warning_threshold` | `true` when available bytes are strictly below the warning threshold. |
| `at_or_below_safety_reserve` | `true` when available bytes are less than or equal to the reserve. |

Byte counts are decimal strings in both text and JSON output. The warning and reserve figures describe this observation. They do not set aside space, identify the disk, or report whether the disk is healthy or encrypted.
