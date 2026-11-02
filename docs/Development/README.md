# Why these Rust types exist

Start with [jobs and retries](Jobs.md) for the `JobLifecycle` example. It explains the problem before the fields or methods. [Storage and reports](Storage.md) covers the snapshot and capacity types. [Earlier foundation types](Foundation.md) explains the IDs, counters, messages, and private helpers those types build on.

A type's documentation should answer three questions: **what real thing does this represent, why does it need to exist, and what mistake does it help prevent?** “Creates a queued lifecycle” only tells you the initial state. It leaves out why a lifecycle is useful.

## Begin with the problem

Suppose you cancel a video conversion just as its worker finishes. The app needs to keep the operation canceled when that worker sends a late result. It also needs to reject an old worker's result if you have already started a replacement run.

`JobLifecycle` keeps the job's stage and the numbers used to reject outdated actions together. Its methods check a requested change before updating those values. Its purpose is to keep one long-running user request's status consistent when events arrive at different times.

`JobLifecycle::new()` makes the initial bookkeeping for that request. Running the conversion, saving the job, and stopping a process are separate responsibilities. The [worked example](Jobs.md#joblifecycle-keeps-a-jobs-status-and-safety-numbers-together) shows exactly what the bookkeeping contains and how it changes.

## The new types, by question they answer

These are the 17 production types introduced by the five recent feature PRs. Some are private helpers; “private” means other crates cannot use them directly.

| Question | Types | Explanation |
|---|---|---|
| What stage is this request in, and may this event change it? | `JobState`, `JobLifecycle`, `JobTransitionError` | [Job state and outdated results](Jobs.md#jobstate-names-the-stage-of-the-work) |
| May this failure be retried, and how long should we wait? | `FailureClass`, `RetryPolicy`, `RetryDecision`, `RetryStop`, `RetryPolicyError` | [Retry decisions](Jobs.md#retry-types-limit-repeated-failures) |
| What did the metadata snapshot capture? | `SnapshotScope`, `SnapshotReport` | [Snapshot descriptions](Storage.md#snapshots-what-did-we-copy) |
| How do we retain the checked source and describe a saved copy? | Private `VerifiedMetadata`, `SnapshotManifest` | [Snapshot implementation helpers](Storage.md#why-the-snapshot-helpers-are-separate-types) |
| How much space is available, and what do the default thresholds say? | `CapacityReport`, `CapacityAssessment`, `CapacityScope` | [Capacity observations](Storage.md#capacity-how-much-room-is-available) |
| Which command did the person type? | Private `SnapshotCommand`, `StorageCommand` | [Command arguments](Storage.md#command-types-describe-what-the-person-asked-for) |

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

This guide was checked against the following code on **27 October 2026**. It adds documentation without bringing the open feature implementations into `main`.

| Area | Code described |
|---|---|
| IDs, counters, protocol, metadata initialization, and snapshots | Already on this documentation PR's `main` base; source links in the guide are relative links into the repository. |
| Job lifecycle | [PR #23](https://github.com/truongmleon/Jumps5art/pull/23), still open when this guide was written. The guide links to its reviewed source revision. |
| Retry policy | [PR #24](https://github.com/truongmleon/Jumps5art/pull/24), still open when this guide was written. |
| Capacity inspection and `StorageCommand` | [PR #25](https://github.com/truongmleon/Jumps5art/pull/25), still open when this guide was written. |

These are foundation libraries and local development tools. An example about importing music or converting video explains the intended use of a type; it does not mean those app features or a durable job scheduler are implemented.
