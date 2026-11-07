# Why these Rust types exist

Start with [jobs and retries](Jobs.md) for the `JobLifecycle` example. It explains the problem before the fields or methods. [Storage and reports](Storage.md) covers the snapshot and capacity types. [Earlier foundation types](Foundation.md) explains the IDs, counters, messages, and private helpers those types build on.

A type's documentation should answer three questions: **what real thing does this represent, why does it need to exist, and what mistake does it help prevent?** “Creates a queued lifecycle” only tells you the initial state. It leaves out why a lifecycle is useful.

## Begin with the problem

Suppose you cancel a video conversion just as its worker finishes. The app needs to keep the operation canceled when that worker sends a late result. It also needs to reject an old worker's result if you have already started a replacement run.

`JobLifecycle` keeps the job's stage and the numbers used to reject outdated actions together. Its methods check a requested change before updating those values. Its purpose is to keep one long-running user request's status consistent when events arrive at different times.

`JobLifecycle::new()` makes the initial bookkeeping for that request. Running the conversion, saving the job, and stopping a process are separate responsibilities. The [worked example](Jobs.md#joblifecycle-keeps-a-jobs-status-and-safety-numbers-together) shows exactly what the bookkeeping contains and how it changes.

## Types by the question they answer

This index groups related types by purpose. Some are private helpers; “private” means other crates cannot use them directly.

| Question | Types | Explanation |
|---|---|---|
| What stage is this request in, and may this event change it? | `JobState`, `JobLifecycle`, `JobTransitionError` | [Job state and outdated results](Jobs.md#jobstate-names-the-stage-of-the-work) |
| May this failure be retried, and how long should we wait? | `FailureClass`, `RetryPolicy`, `RetryDecision`, `RetryStop`, `RetryPolicyError` | [Retry decisions](Jobs.md#retry-types-limit-repeated-failures) |
| What request and outcome survive command exit? | `SnapshotJob` | [Saved requests](Jobs.md#snapshotjob-keeps-the-request-and-outcome-after-exit) |
| What changed since a saved position? | `SnapshotJobChange`, `SnapshotJobChangePage` | [Change pages](Jobs.md#change-pages-follow-commits-across-jobs) |
| What did the metadata snapshot capture? | `SnapshotScope`, `SnapshotReport` | [Snapshot descriptions](Storage.md#snapshots-what-did-we-copy) |
| How do we retain the checked source and describe a saved copy? | Private `VerifiedMetadata`, `SnapshotManifest` | [Snapshot implementation helpers](Storage.md#why-the-snapshot-helpers-are-separate-types) |
| How much space is available, and what do the default thresholds say? | `CapacityReport`, `CapacityAssessment`, `CapacityScope` | [Capacity observations](Storage.md#capacity-how-much-room-is-available) |
| What happened during an explicit schema upgrade? | `MigrationReport`, private `MigrationJournal` | [Recoverable upgrades](Storage.md#upgrades-bridge-the-database-and-format-marker) |
| Which command did the person type? | Private `SnapshotCommand`, `StorageCommand`, `JobCommand` | [Command arguments](Storage.md#command-types-describe-what-the-person-asked-for) |

## Reading `struct`, `enum`, and `impl`

These words describe different parts of the code:

| Rust syntax | Meaning here | Example |
|---|---|---|
| `struct` | Defines values that belong together. | A `JobLifecycle` contains a stage, revision, and worker token. |
| `enum` | Defines a fixed set of named alternatives. An alternative can also carry data. | A `RetryDecision` is either a wait with its duration or a stop with its reason. |
| `impl TypeName` | Groups functions and methods belonging to an already defined type. | `impl JobLifecycle` contains the operations for creating and changing its bookkeeping. |
| `new()` | A conventional function name for constructing a fresh value; its exact effect depends on the type. | `JobLifecycle::new()` creates the initial bookkeeping. It takes no arguments. |
| `-> Self` | Returns the type named by the surrounding `impl`. | Inside `impl JobLifecycle`, `Self` means `JobLifecycle`. |

The `struct JobLifecycle` declaration tells you what the value holds. The `impl JobLifecycle` block tells you what operations it supports. Read the type's purpose first, then the method you are reviewing.

## What is available in this checkout?

This guide describes the code in this checkout. Job lifecycle, retry policy, capacity observations, and metadata upgrades are merged into main. The durable snapshot-job, history, filtering, recorded-completion, and change-page features are included through the preceding PR stack. Source links are relative to the checkout so they follow the code being reviewed.

For runnable examples and current command limits, use the [CLI reference](../CLI/README.md) and [development instructions](../../CONTRIBUTING.md). A documentation page or successful policy test does not establish completion of the Foundation release stage.

These are foundation libraries and local development tools. An example about importing music or converting video explains the intended use of a type; it does not mean those app features or a durable job scheduler are implemented.
