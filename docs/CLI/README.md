# jumpstart-server commands

`jumpstart-server` is the local storage tool in this repository. From the repository root:

```sh
cargo run -p jumpstart-server -- --help
```

These are the commands the binary accepts. [CONTRIBUTING.md](../../CONTRIBUTING.md) describes the same preview in more detail, including how initialization and snapshots are published. The finished server's administrative commands are specified separately in [Technical Design.md](<../../Technical Design.md>) and are absent from `--help`.

The commands run on Linux and macOS. On Windows they exit unsuccessfully without changing files. The `health` report identifies unavailable checks there.

## Available commands

| Command | What it does |
|---|---|
| `init` | Creates a new metadata folder that identifies one server. |
| `serve` | Starts the Linux-only local administration daemon. |
| `status` | Reads the running daemon's metadata status through its private socket. |
| `inspect` | Reads an existing metadata folder and prints that identity. The folder stays unchanged. |
| `health` | Queries the running Linux daemon, or checks an offline root with `--metadata-root PATH`. |
| `snapshot create` | Copies that folder's bookkeeping database into a new checked bundle. |
| `snapshot verify` | Checks an existing bundle. The original metadata folder is not required. |
| `migrate` | Upgrades older metadata after saving a verified recovery copy; see [migration instructions](../../CONTRIBUTING.md#recoverable-metadata-schema-upgrade). |
| `jobs` | Enqueues/runs snapshot requests, reads their status/history/change pages, cancels queued requests, and recovers interrupted work; see [job commands](../../CONTRIBUTING.md#persisted-local-snapshot-jobs). |
| `jobs run-next` | Runs one oldest queued snapshot; JSON returns a saved `job` or `null` for an empty queue. A saved failed outcome exits unsuccessfully. |
| `storage inspect` | Reports how much space the filesystem has for an existing directory. |
| `storage register PATH --plan --role primary\|backup` | Observes a proposed volume through the Linux daemon. |
| `update plan` | Verifies a signed Linux release directory and reports compatibility with the running daemon. |

`init`, `migrate`, and `snapshot create` require `--development`. Job enqueue/run/run-next/cancel/recover also require `--development`; job list/inspect/history/changes are read-only and need no acknowledgment.

A successful command prints its report on stdout and exits successfully. A failure prints `error:` and the reason on stderr and exits with a failure status. Failures normally leave stdout empty. A snapshot job that records `Failed` prints that saved outcome, and `health` prints any available evidence before reporting an incomplete check. Add `--json` for JSON. `jobs changes` uses compact JSON to preserve its response byte bound, while other commands use pretty-printed JSON. Without `--json`, the first line is a heading and the fields follow, indented two spaces per level.

`--help` and `--version` are handled before a command runs. `snapshot --help`, `storage --help`, and `jobs --help` list the nested commands.

## `serve` and `status`

On Linux, use the dedicated service user to run `serve --metadata-root PATH`. The systemd unit template in [packaging/linux](../../packaging/linux/jumpstart.service) expects that user, the installed binary, and a root-owned `/etc/jumpstart/server.env` containing `JUMPSTART_METADATA_ROOT`. It creates the private runtime directory for the default socket at `/run/jumpstart/admin.sock`.

Run `status --json` as root or as the service user. For a scratch run, both commands accept `--socket PATH` pointing into an existing private directory owned by the service user. The server rejects root execution and checks the caller's OS credentials before decoding a request.

The daemon takes exclusive metadata ownership for its lifetime. Offline commands require stopping it first. Status reports `metadata_verified` after successful startup verification, or `recovery_required` if the root needs attention. Missing roots stay absent. Stop with SIGINT or SIGTERM for orderly release of the lock and socket.

## `init`

```sh
cargo run -p jumpstart-server -- init --metadata-root /tmp/jumpstart-metadata-demo --development
```

| Flag | Required | Meaning |
|---|---|---|
| `--metadata-root` | yes | New directory. Its parent must already exist. |
| `--development` | yes | Acknowledges that this is a development preview. Production volume checks, encryption, backup, and restore are still unimplemented. |
| `--json` | no | Print the report as JSON. |

The new folder receives `format.json`, `metadata.lock`, and `database/jumpstart.sqlite`. The database records the server ID, the sync epoch, the format version, and the applied migrations. It starts with an empty application catalog and job queue. The identity values are generated once for this folder.

The path must be new. An existing directory, including an empty one, and an existing symlink are left unchanged and the command fails. Preparation happens in a private staging directory beside the destination. The folder is renamed into place only after the database and `format.json` agree. If preparation fails, that staging directory is kept and named in the error.

Heading: `Verified offline development metadata:`

| Field | JSON value |
|---|---|
| `root` | Absolute path of the published folder |
| `server_id` | UUID string for this server |
| `sync_epoch` | UUID string recorded with the server ID |
| `format_version` | Number, currently `1` |
| `schema_version` | Number, currently `4` |
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

## `health`

```sh
cargo run -p jumpstart-server -- health --metadata-root /tmp/jumpstart-metadata-demo --json
```

With `--metadata-root PATH`, the scope is `offline_metadata`. The `metadata` component reports the same integrity and schema evidence as `inspect`, or explains why verification was refused. The `capacity` component independently reports observed bytes and default thresholds on the filesystem containing the supplied directory. It can report `low` or `at_or_below_reserve` even when the metadata verifies.

Without `--metadata-root`, `health --json` uses the protected local socket. The `live_local_admin` report measures space now and shows saved snapshot-job counts by state. It labels the database integrity report as startup evidence and rechecks that the leased root, database, and lock still match the current paths. Disk hardware and registered-volume status are shown as unknown until their probes are available. Use `--socket PATH` to query a scratch daemon.

If metadata or capacity cannot be checked, the command prints its component report and exits unsuccessfully. A measured capacity warning alone exits successfully. Both modes are read-only.

## `migrate`

```sh
cargo run -p jumpstart-server -- migrate --metadata-root /tmp/jumpstart-metadata-demo --snapshot-output /tmp/jumpstart-before-schema-4 --development --json
```

| Flag | Required | Meaning |
|---|---|---|
| `--metadata-root` | yes | Existing offline metadata folder. |
| `--snapshot-output` | yes | New directory outside the metadata folder. Use the original path when resuming an interrupted migration. |
| `--development` | yes | Acknowledges the schema upgrade and limited scope of the recovery copy. |
| `--json` | no | Print the report as JSON. |

The command upgrades a verified older root to schema 4 after saving a verified pre-upgrade snapshot. A current root reports both versions as 4 and `backup: null`. A journal from an older binary finishes its recorded target. Run a second migration with a fresh snapshot path to reach schema 4. See [recovery and retry details](../../CONTRIBUTING.md#recoverable-metadata-schema-upgrade).

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

`change_seq` is always present. Schema 1 records JSON `null` because it has no stored watermark. Schemas 2–4 record the stored value as a decimal string, including `"0"` for an initial position.

Heading: `Verified local metadata snapshot (not a full backup):`

| Field | JSON value |
|---|---|
| `root` | Absolute path of the published bundle |
| `snapshot_format_version` | Number, currently `1` |
| `scope` | `"bootstrap_metadata"` for schemas 1/2, `"metadata"` for schemas 3/4 |
| `server_id` | Server UUID copied from the source |
| `sync_epoch` | Sync-epoch UUID copied from the source |
| `format_version` | Source metadata format, currently `1` |
| `schema_version` | Source schema, from `1` through `4` |
| `change_seq` | Stored decimal string, or `null` for schema 1 |
| `database_bytes` | Decimal string, the exact length of `metadata.sqlite` |
| `database_hash` | `b3:` followed by 64 lowercase hex digits |

`bootstrap_metadata` covers initial server bookkeeping in schemas 1/2. Schemas 3/4 use `metadata` and include saved snapshot jobs and their changes. Schema 4 also includes the shared catalog and the Worlds recovery index. The bundle excludes media bytes, Worlds world files and recovery bundles, secrets, and logs. There is no command here that turns the bundle back into a metadata folder or a running server. Keeping the bundle on the same disk as the source does not protect either copy if that disk fails.

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

## `jobs`

All actions use `--metadata-root PATH` and accept `--json`. Enqueue, run, run-next, cancel, and recover also require `--development`.

| Action | Additional options | Result |
|---|---|---|
| `enqueue-snapshot` | `--output PATH` | Save a request for a new output directory and return its job ID. |
| `run` | `--job-id ID` | Execute the queued job or return its recorded completion. |
| `run-next` | | Execute the oldest queued job. An empty queue succeeds and returns `{"job":null}` in JSON. |
| `list` | Optional `--state STATE`, `--limit N` (1–100, default 50) | Read the current records in acceptance order. |
| `inspect` | `--job-id ID` | Read one current record and its saved outcome. |
| `history` | `--job-id ID` | Read one job's historical revisions. |
| `changes` | `--epoch UUID`, `--after POSITION`, optional `--limit N` (1–500, default 100) | Read a bounded page of saved job changes after a cursor. |
| `cancel` | `--job-id ID`, `--expected-revision REVISION` | Cancel a job that is still queued and at the inspected revision. |
| `recover` | | Record interrupted active jobs as failed and retain their artifacts. |

Saved run failures print a job record and return nonzero. See [the snapshot job walkthrough](../../CONTRIBUTING.md#persisted-local-snapshot-jobs) for examples and interrupted-work recovery.

## Volume registration preflight

Run `storage register /absolute/path --plan --role primary --json` with the
Linux daemon running. The report describes the actual mount, directory identity,
filesystem, read-only state, and any existing files or marker. Symlinks in
the supplied path are refused. Encryption and registered-device topology remain
unverified. This preflight creates no marker or approval record.

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

## `update plan`

```sh
jumpstart-server update plan --release-root /absolute/candidate-release --json
```

The Linux command asks the local daemon for its verified format and schema
versions. It reads the root-installed `/etc/jumpstart/release.pub` (32 raw
Ed25519 public-key bytes), verifies `release.json` against its detached
`release.sig` (64 raw bytes), and checks every listed artifact's size and
BLAKE3 identity. It reports whether the release can read and write the
current storage. The plan leaves installation and data paths untouched.
