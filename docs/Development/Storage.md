# Why storage uses reports, scopes, and helpers

[Back to the type guide](README.md)

The storage library currently prepares and checks a server's bookkeeping folder and makes standalone copies of its SQLite metadata database. A separate capacity feature observes available disk space. The types below keep the observations, their meaning, and the resources needed to make them separate.

## Snapshots: what did we copy?

Suppose you create a metadata snapshot and later move it to another folder. You need to know which server it came from, whether its files still match the recorded contents, and what was included. A directory name such as `backup-1` cannot answer those questions.

The snapshot contains `metadata.sqlite` and `snapshot.json`. The database holds server bookkeeping, saved snapshot jobs and their ordered changes in schema 3 and later, and the application catalog in schema 4. Schemas 1/2 retain the earlier bootstrap scope. It does not contain songs, photos, Worlds data, or secrets.

| Type | What it is for | Why we need it |
|---|---|---|
| `SnapshotReport` | The answer returned after creating or verifying a snapshot: its current path, source identity, versions, database size, fingerprint, and scope. | The CLI and future callers need one consistent description of the result. A bare success flag would hide which server and exact artifact were checked. |
| `SnapshotScope` | A machine-readable label for what the artifact includes. It uses `BootstrapMetadata` for schemas 1/2 and `Metadata` for schemas 3/4. | A successful metadata copy must identify its actual contents. Otherwise callers might describe it as a complete server backup and assume missing media were protected. |
| `MetadataRestoreReport` | Groups the verified input snapshot, the recovered root's new identity, and the count of old snapshot jobs kept in the input. | The original and recovered roots have different authorities. Returning both makes it clear which digest was used, which root is ready to inspect, and why the old jobs cannot replay there. |
| `SnapshotBackupInventory` and `PrimaryBackupInventory` | Describe the captured catalog and the retained resources each primary must preserve. | Two resources can share the same immutable bytes on one primary. Counting the unique blob separately on each primary captures storage work without counting it twice for two versions on the same primary. |

The report uses existing foundation types for individual fields:

| Field type | Meaning in this report |
|---|---|
| `ServerId` | Which server's bookkeeping was captured. |
| `SyncEpoch` | Which synchronization history that server used. |
| `Counter` | Exact database byte count, written as a decimal string in JSON. |
| `BlobId` | A fingerprint of the copied database bytes, used when checking that they still match. |
| `Option<Counter>` | A change position if one is available. Bootstrap schema 1 has none, so `change_seq` is `None` in Rust and `null` in JSON. |

The unavailable change position is explicit so future synchronization code cannot mistake it for a captured position of zero. Fingerprints check the artifact against its manifest; they do not authenticate who supplied the bundle. Restoring a server is separate work.

### Why the snapshot helpers are separate types

These two private types serve different parts of the operation:

| Type | What it is for | Why a report alone would not do the job |
|---|---|---|
| `VerifiedMetadata` | Keeps the checked source connection, its identity information, and its maintenance-lock handle alive during capture. | Copying an identity report and closing the source would release its lock. A cooperating writer could then modify the source before a later copy. Holding this value retains the resources used to protect the read. |
| `SnapshotManifest` | The description saved in `snapshot.json`, including identity, schema, scope, byte count, and fingerprint. | A later verifier must have something to compare with the database even after the original server is gone. A report returned to a caller would disappear when that caller exits. |

`SnapshotManifest` and `SnapshotReport` share several fields because they describe the same captured database, but they serve different readers. The manifest is the saved description that the verifier must check. The report is the result returned to the caller, including the bundle's current local path. Moving a bundle changes that path without changing its recorded source identity.

“Holding” `VerifiedMetadata` has a concrete Rust consequence: its connection and lock handle remain owned by that value until it is dropped, normally when it goes out of scope. The struct groups those resources so the capture code can retain them together.

Source: [snapshot report and scope](../../crates/jumpstart-storage/src/lib.rs), [source guard](../../crates/jumpstart-storage/src/platform.rs), and [saved manifest](../../crates/jumpstart-storage/src/platform/snapshot.rs).

## Capacity: how much room is available?

Capacity observations are implemented in this checkout.

Suppose a filesystem has 100 GiB total capacity and reports 8 GiB available to ordinary users. The measurement answers “how much room is there right now?” The default policy then says that 8 GiB is below its 10 GiB warning threshold and calculates a 256 MiB recovery reserve.

| Type | What it is for | Why we need it |
|---|---|---|
| `CapacityReport` | One observation of a directory's filesystem: path, total bytes, free bytes, ordinary-user available bytes, scope, and assessment. | Measurements must stay associated with the path and the observation they came from. Free space can change between calls, and some free space can be unavailable to ordinary users. |
| `CapacityAssessment` | The warning threshold, safety reserve, available bytes above that reserve, and whether the supplied measurement crosses those boundaries. | The decision rules should be consistent for every caller. They can also be tested with fixed numbers without filling a real disk. |
| `CapacityScope` | Labels the result `CapacityOnly`. | Available space answers one storage question. The label stops consumers from confusing that observation with checks of device health, encryption, registered identity, or permission to write. |

The default reserve leaves room for failure recording, safe shutdown, and recovery state when ordinary work fills a volume. `assess_capacity` calculates that policy from byte counts. `inspect_capacity` measures a real directory's filesystem and includes the resulting assessment in a report.

The current report observes and calculates. It does not set aside bytes or pause a producer; future job admission must enforce those decisions. Another process can consume space immediately after measurement.

Source: [capacity types and calculations](../../crates/jumpstart-storage/src/capacity.rs).

## Upgrades bridge the database and format marker

A metadata upgrade changes two separately stored things: the SQLite schema and `format.json`. A database transaction cannot atomically replace that separate marker file. An interruption can leave the database upgraded while the marker still identifies the older layout.

| Type | What it is for | Why it exists |
|---|---|---|
| `MigrationReport` | Returns the checked resulting metadata, starting/target schema versions, retained backup, and whether work resumed. | A caller must distinguish a real upgrade, a resumed upgrade, and an already-current no-op, and locate the verified prior state. |
| Private `MigrationJournal` | Retains the workflow's identities, source/target versions, baseline and verified recovery-copy information. | A later explicit retry needs evidence of the exact upgrade to finish; it cannot guess from a mismatch or reuse an unrelated snapshot. |

Migration first saves a verified recovery snapshot, then records the journal before changing the source. With a journal present, ordinary inspection refuses recovery work. Repeating migration with the same paths validates the recorded evidence and finishes the interrupted workflow. Schema 3 preserves the earlier change position as its history floor. Schema 4 adds the catalog while preserving the job ledger and its history. Both upgrades retain the server identity and epoch. Completing a legacy journal keeps its recorded target; another forward upgrade needs its own recovery copy.

Source: [upgrade report](../../crates/jumpstart-storage/src/lib.rs), [journal and recovery](../../crates/jumpstart-storage/src/platform/migration.rs), and [migration tests](../../crates/jumpstart-storage/tests/migrations.rs).

## Existing metadata and error types

| Type | What it is for | Why we need it |
|---|---|---|
| `MetadataReport` | The answer from initializing or inspecting a metadata root: its path, server identity, epoch, versions, and database-check results. | Callers need to know which installation was checked, rather than receiving an unexplained “OK.” It describes the live bookkeeping root; `SnapshotReport` describes a separate copied artifact. |
| Private `FormatMarker` | The fields saved in the root's `format.json`. | The folder and its database must agree about identity and versions. Checking both can detect a database copied into the wrong root or a mismatched layout. |
| `StorageError` | A specific reason storage work failed, often including a path and underlying cause. | “Already exists,” “writer active,” and “corrupt snapshot” require different responses. A retained staging path also lets an operator inspect incomplete work instead of losing it. |
| `Result<T>` in this crate | A shorter spelling of Rust's `Result<T, StorageError>`. | Storage functions can state the successful value once while sharing the same error family. This alias does not create a new kind of runtime object. |

A successful storage function returns a report after its checks. Constructing or deserializing a report yourself does not repeat those checks or grant permission to write; the public report fields are descriptions, not authority tokens. The private source guard is responsible for retaining the actual open connection and lock during capture.

An error after publication can leave a complete destination in place. For example, the final parent-directory synchronization may fail after the rename. Callers must inspect that result before retrying; `Err` does not always mean “nothing was created.”

Source: [metadata, marker, errors, and result alias](../../crates/jumpstart-storage/src/lib.rs).

## Command types describe what the person asked for

When a person runs a command, the parser turns argument strings into one of these private types before the storage code runs:

| Type | What it is for | Why we need it |
|---|---|---|
| `Cli` | Holds the parsed invocation. | The rest of the program can receive one structured request rather than repeatedly interpret raw argument strings. |
| `Command` | Chooses the top-level action, such as initialization, metadata inspection, or snapshot work, together with its arguments. | Different actions require different data. An initialization request should carry its new path and development acknowledgment together. |
| `SnapshotCommand` | Chooses snapshot creation or verification. | Creation needs a source and new output directory; verification needs an existing bundle. Keeping them as alternatives makes the required arguments clear. |
| `JobCommand` | Groups enqueue, run, read, cancel, and recovery actions with their typed arguments. | A cancellation needs an observed revision; change-page reads need an epoch and position. Keeping those fields with their action makes required inputs explicit. |
| `StorageCommand` | Groups storage observations, currently capacity inspection. | Observing an ordinary folder's space is a different request from checking an initialized Jumpstart metadata root. This type gives `storage inspect PATH` its own argument shape. |

The parser creates values describing the request. The `run` function then chooses which storage function to call; the enum itself performs no copying or inspection.

Source: [current command parsing](../../crates/jumpstart-server/src/main.rs) and [`StorageCommand` in PR #25](https://github.com/truongmleon/Jumps5art/blob/7d6e2577f4eb38307963f5b4cf6dc8cf922568d9/crates/jumpstart-server/src/main.rs).
