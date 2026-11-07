# Why jobs and retries have these types

[Back to the type guide](README.md)

A **job** is one long-running request from the user, such as “convert this video” or “import these songs.” A **worker** is the code or process doing that work. An **attempt** is one run of that worker; the same job may need another attempt after a pause or a temporary failure.

Several events can arrive at different times: a person cancels while a worker finishes, an app sends an action based on an old screen, or a provider keeps failing. The types below provide rules for handling those events consistently.

The shared lifecycle and retry rules operate in memory. The storage crate now uses the lifecycle to save and execute local metadata-snapshot requests. Other job kinds, external worker supervision, retry timers, and app publication still require implementation. The local runner implements queued/running/verifying/completed/failed/canceled; the additional policy states below do not make pause or resume available in that runner.

## `JobState` names the stage of the work

**What it is for:** a fixed vocabulary for where the user's request has reached. For a video conversion, `Running` describes active conversion, `Verifying` describes output awaiting acceptance checks, and `Completed` describes the application's accepted success.

**Why we need it:** all callers should agree on those stages. A string can contain a typo or a different label for the same stage. The enum provides named choices; `can_transition_to` checks the allowed moves between them.

| State | Meaning for the user's work |
|---|---|
| `Queued` | Waiting to be admitted to run. |
| `Running` | The current attempt is doing work. |
| `Paused` | Work has stopped at a safe checkpoint. |
| `WaitingForSource` | A needed device, disc, volume, or other source is unavailable. |
| `WaitingForStorage` | The destination or required space is unavailable. |
| `WaitingForInput` | Credentials, a decision, or another user action is needed. |
| `Verifying` | Output exists and still needs acceptance and publication. |
| `Completed`, `Failed`, `Canceled` | Finished under this job's identity. A retry or resume needs a successor job. |

The enum alone describes a stage. The extra information needed to reject an outdated action belongs to `JobLifecycle`.

## `JobLifecycle` keeps a job's status and safety numbers together

**What it is for:** controlling changes to one request's status, including changes reported by its current worker.

**Why we need it:** cancellation must remain effective when an old result arrives. A replacement worker must not be overwritten by an earlier worker. A control action based on an old screen must be detected. The type keeps the following values together and checks them before changing anything:

| Field | What it means | Example of the problem it addresses |
|---|---|---|
| `state` | The current `JobState`. | A canceled conversion must stay canceled when a late result arrives. |
| `revision` | A number advanced after every accepted state change. | An app sends a control action based on a version of the job it saw before another change. |
| `attempt_generation` | A numbered token identifying which worker's reports remain eligible. | Worker 1 reports after work has paused and a replacement worker has started. |

The fields are private. Callers use methods rather than changing the stage while forgetting to update its safety numbers. `transition` checks the control path's expected revision; `worker_transition` checks the reporting worker's token and its allowed actions.

Here is one supported sequence. Each accepted control action uses the current revision. “Record” means updating the in-memory bookkeeping; the application performs the corresponding real work separately.

| Event | State after the event | Revision | Worker token counter |
|---|---|---:|---:|
| Construct a fresh lifecycle | `Queued` | 1 | 0 |
| Application records the start of an attempt | `Running` | 2 | 1 |
| Worker 1 reports output ready for checking | `Verifying` | 3 | 1 |
| Application records the user's cancellation | `Canceled` | 4 | 2 |
| Worker 1 reports again | Rejected; remains `Canceled` | 4 | 2 |

The revision advances for every accepted transition. The worker token stays at 1 when that same attempt enters verification. Cancellation advances the token counter to invalidate the old worker's authority to report.

This invalidation is called **fencing** in the source. It means the application stops accepting that worker's results. Stopping its actual process and cleaning up its files need separate implementation.

The token counter is **not an attempt count**. Starting a worker advances it, but invalidating a worker also advances it. For example, starting, pausing, requeuing, and starting again use counter values `1 → 2 → 2 → 3`. The replacement worker gets token 3, so worker 1's report fails even while the replacement is active.

The counters belong to one lifecycle. This type does not store a `JobId`; application code must bind each reported token to the correct job before using it. Two different jobs can both have token 1.

`JobLifecycle::new()` therefore creates the initial bookkeeping for protecting one long-running request from invalid or outdated status changes. Its initial values are `Queued`, revision 1, and token counter 0. It takes no arguments and returns a `JobLifecycle`; it does not create a database record or start a worker.

## `JobTransitionError` explains a refused status change

**What it is for:** telling the caller why the lifecycle rejected a request, while leaving the existing values unchanged.

**Why we need it:** the next action depends on the reason. An old screen needs refreshed information; an old worker's report should be discarded. Code should be able to distinguish those cases without interpreting an error sentence.

| Error case | What happened |
|---|---|
| `RevisionMismatch` | A control request supplied a different revision from the current one. |
| `InvalidTransition` | The requested move is absent from the allowed state table. |
| `NoActiveAttempt` | A worker reported while the job was outside `Running` or `Verifying`, for example after cancellation. |
| `StaleAttempt` | A worker reported with a token different from the active one. |
| `WorkerTransitionForbidden` | A worker requested an action reserved for the control path or otherwise unavailable to workers. |
| `RevisionExhausted`, `AttemptGenerationExhausted` | A safety number cannot be advanced within the supported range. |

Workers can report output ready for verification, failure, or permitted waiting states. Only the application's control path can record `Completed`, after it has independently checked and published the output. Calling that method is not itself an output check.

Source: [job types](../../crates/jumpstart-domain/src/jobs.rs). Review the cancellation, replacement-worker, and trusted-completion cases in the [job tests](../../crates/jumpstart-domain/tests/job_lifecycle.rs).

## Retry types limit repeated failures

A job's stage and its retry allowance answer different questions. The lifecycle controls status changes. The retry policy decides whether another automatic attempt is allowed and what minimum wait to use.

For example, an unavailable music provider may recover. Missing sign-in credentials need user input. Repeating both requests in the same way forever would waste work without solving the second problem.

| Type | What it is for | Why we need this type |
|---|---|---|
| `FailureClass` | Categorizes a failure as temporary, source-changed, input-required, unsupported, corrupt, or permanent. | The retry policy needs a cause supplied by the caller. It should not infer whether to retry from an arbitrary error message. Only `Transient` allows automatic retry. |
| `RetryPolicy` | Remembers retry slots used, elapsed-time observations, and the first stop reason for one attempt group. | A limit requires memory of earlier decisions. A stopped group must stay stopped when a later call supplies a shorter provider wait. |
| `RetryDecision` | Returns `Wait { delay, retry_number }` or `Stop(reason)`. | Each action carries its required information. A wait has a duration; a stop has an explanation. |
| `RetryStop` | Names the reason automatic work must stop: a nontransient failure, six retries used, or exhausted time budget. | The application needs to explain the stopped outcome and decide what user action is appropriate. |
| `RetryPolicyError` | Identifies invalid policy inputs, such as time moving backward or a random delay outside its allowed range. | A bad call to the policy is a different problem from the provider failure being evaluated. It leaves the policy unchanged so the caller can correct the input. |

An **attempt group** contains the initial attempt and up to six automatic retries. Its 15-minute elapsed-time budget includes work and waiting. An explicit user retry starts a new policy with a fresh time origin.

For example, the first attempt fails after ten seconds. The caller supplies `Transient`, a valid one-second random delay, and the provider's requested minimum wait of 30 seconds. The policy returns a wait of 30 seconds with retry number 1 and consumes one slot. The caller must arrange that wait before trying again.

For missing sign-in credentials, the caller supplies `InputRequired`. The policy returns `Stop(NonTransient(InputRequired))` without consuming a retry slot.

A stop is a valid policy decision, returned as `Ok(RetryDecision::Stop(...))`. `Ok` means the policy evaluated its inputs successfully; the import has not succeeded. `Err(RetryPolicyError::...)` means the inputs to the policy were invalid.

The policy receives elapsed time and a random delay from its caller. It reads no clock, draws no randomness, and starts no timer or worker. Its relationship with `JobLifecycle` must be implemented by future scheduling code; constructing the two values does not connect them automatically.

Source: [retry types](../../crates/jumpstart-domain/src/retry.rs) and [retry tests](../../crates/jumpstart-domain/tests/retry_policy.rs).

## `SnapshotJob` keeps the request and outcome after exit

`JobLifecycle` enforces transition rules in memory. `SnapshotJob` is the saved record that lets another command find the request after the accepting process exits. It combines the job ID and chosen output path with the recorded state, revision, worker token, acceptance position, observation times, and eventual result or error. Saving a request does not start its copy.

For example, enqueue request A, then request B, then run A:

| Global change position | Job | Its revision | Saved state |
|---:|---|---:|---|
| 1 | A | 1 | Queued |
| 2 | B | 1 | Queued |
| 3 | A | 2 | Running |
| 4 | A | 3 | Verifying |
| 5 | A | 4 | Completed |

A's revision counts changes to A. The global position orders commits across both requests. Its worker token identifies eligible reports, as in the lifecycle example above. These three numbers answer different questions.

The storage engine updates the current row, full historical projection, and global position in one transaction. On inspection, it checks that the saved history can be replayed through `JobLifecycle`. This connects the policy to persistence rather than trusting an arbitrary saved status string.

`result: Option<SnapshotReport>` is absent before completion. A completed report records the snapshot accepted at that time; it does not prove the artifact is healthy today. Repeating `jobs run` for a completed request returns the same saved record. Use `snapshot verify` to check the artifact at its current location. A saved failed record means the work failed even though the API successfully returned that record.

`jobs history` returns one job's original projections. `jobs list --state queued` finds waiting requests before applying the result limit. Reads never execute the requests. Running holds the exclusive maintenance lock, so these offline commands cannot monitor a concurrent active copy. After a process interruption, explicit recovery fences incomplete active work as failed while preserving a committed completion and retained artifacts.

Source: [saved job fields and public APIs](../../crates/jumpstart-storage/src/lib.rs), [transaction and runner](../../crates/jumpstart-storage/src/platform/jobs.rs), and [history reader](../../crates/jumpstart-storage/src/platform/history.rs).

## Change pages follow commits across jobs

A reader that has seen changes through position 2 needs A's running, verifying, and completed records from the table above. Reading A's current row three times would lose the earlier states.

| Type | What it is for | Why it exists |
|---|---|---|
| `SnapshotJobChange` | One global sequence, projection-format version, and original full `SnapshotJob`. | Associates each past value with the commit that accepted it. The projection-format version is separate from the database schema version. |
| `SnapshotJobChangePage` | A bounded group of changes with authority, epoch, and continuation bounds. | Lets a reader catch up in steps without allocating the entire history or silently skipping unavailable records. |

With `after=2` and a two-record limit, the first page includes positions 3 and 4. Its `next=4` tells the reader where to continue; `high_water=5` says there is more at the captured position. The next pull after 4 returns position 5. An empty page at the high-water mark keeps `next` equal to `after`.

The server ID identifies the authority; `sync_epoch` identifies its history. A number from a different epoch cannot safely continue this one. `minimum` is the lowest usable `after` position, inclusive. A migrated root can retain a nonzero floor without inventing earlier job changes. A cursor below that floor needs a verified baseline; moving the number forward alone would omit state the reader never obtained. Future cursors are refused too.

The API returns at most 500 whole records and 1 MiB of compact JSON including the page fields. The byte limit can shorten a page before the row limit does. A record omitted for space never advances `next`. Full ledger verification still happens, so a small returned page does not imply a constant amount of database validation.

This is local inspection of snapshot jobs. It does not apply a client cache, acknowledge a device cursor, compact history, or expose authenticated network synchronization.

Source: [page types](../../crates/jumpstart-storage/src/lib.rs), [bounded reader](../../crates/jumpstart-storage/src/platform/changes.rs), and [cursor tests](../../crates/jumpstart-storage/tests/job_changes.rs).

## How to review a change to these types

Trace one user action through its data, rather than starting with unfamiliar syntax. For cancellation, check which method receives the action, which revision or token it checks, and what happens to a late result. For retry, check which failure class is supplied, which earlier decisions the policy remembers, and who performs the returned wait.

Then look for a test of the consequence: a late result leaves a canceled job unchanged, or a provider's minimum wait is preserved. Those examples explain why the fields and separate error/decision types exist.
