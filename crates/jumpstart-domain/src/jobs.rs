//! Checks how long-running work may move between queued, active, and finished states.
//!
//! For example, a video conversion must pass through verification before completion.
//! If the user cancels it first, an old worker's later completion report must fail.
//! A revision detects stale control requests; an attempt generation is a numbered
//! token that distinguishes an active worker run from an older, fenced-out run.
//!
//! This file implements those rules in memory. It does not save jobs, launch or
//! stop workers, verify media, or delete output; those actions need application code.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Counter, PositiveCounter};

/// The shared lifecycle states from the technical design's job transition table.
///
/// Wire names use snake case, including `waiting_for_source`. A terminal state
/// cannot be reopened: retrying completed, failed, or canceled work needs a new job.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted work waiting for admission to run.
    Queued,
    /// An active attempt is doing the work.
    Running,
    /// A user or resource decision stopped work at a safe checkpoint.
    Paused,
    /// A required device, disc, volume, or other source is unavailable.
    WaitingForSource,
    /// Destination identity, space, or a storage reservation is unavailable.
    WaitingForStorage,
    /// A credential, decision, or other user-supplied input is required.
    WaitingForInput,
    /// Output exists but has not passed acceptance and publication checks.
    Verifying,
    /// Work finished successfully and cannot restart under the same lifecycle.
    Completed,
    /// Work failed; retry needs a successor job.
    Failed,
    /// Work was canceled; any later worker result is no longer eligible.
    Canceled,
}

impl JobState {
    /// Checks one edge against the design's exact lifecycle transition table.
    ///
    /// # Arguments
    /// - `self`: the current `JobState`, copied by value.
    /// - `next`: the proposed `JobState`.
    ///
    /// # Returns
    /// `true` for a legal edge, or `false` for self-transitions, terminal-state
    /// changes, and every other edge absent from the design table.
    pub fn can_transition_to(self, next: Self) -> bool {
        use JobState::*;
        match self {
            Queued => matches!(
                next,
                Running | WaitingForSource | WaitingForStorage | WaitingForInput | Canceled
            ),
            Running => matches!(
                next,
                Paused
                    | WaitingForSource
                    | WaitingForStorage
                    | WaitingForInput
                    | Verifying
                    | Failed
                    | Canceled
            ),
            Paused => matches!(next, Queued | Canceled),
            WaitingForSource | WaitingForStorage => {
                matches!(next, Queued | WaitingForInput | Failed | Canceled)
            }
            WaitingForInput => matches!(next, Queued | Failed | Canceled),
            Verifying => matches!(
                next,
                Completed | WaitingForStorage | WaitingForInput | Failed | Canceled
            ),
            Completed | Failed | Canceled => false,
        }
    }

    /// Identifies states that require a successor job for any retry or resume.
    ///
    /// # Arguments
    /// - `self`: the `JobState` to examine.
    ///
    /// # Returns
    /// `true` only for `Completed`, `Failed`, and `Canceled`.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// In-memory job state with revision checks and worker-attempt fencing.
///
/// Its private fields change together only after all checks succeed. The revision
/// starts at one; generation zero means no worker has started. A generation is
/// scoped to this lifecycle and grants no authority to operate on a different job.
/// Deserializing or restoring arbitrary persisted lifecycle state is outside this API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobLifecycle {
    /// Current lifecycle state.
    state: JobState,
    /// Revision incremented by every accepted transition.
    revision: PositiveCounter,
    /// Attempt token counter, advanced when starting or fencing a worker run.
    attempt_generation: Counter,
}

/// Explains a rejected lifecycle change without changing the existing state.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JobTransitionError {
    /// The caller observed an older or otherwise different job revision.
    #[error("job revision mismatch: expected {expected}, actual {actual}")]
    RevisionMismatch {
        expected: PositiveCounter,
        actual: PositiveCounter,
    },
    /// The design's transition table does not permit this state change.
    #[error("job transition from {from:?} to {to:?} is not allowed")]
    InvalidTransition { from: JobState, to: JobState },
    /// No running or verifying attempt can supply a worker result in this state.
    #[error("job has no active worker attempt in {state:?}")]
    NoActiveAttempt { state: JobState },
    /// The result belongs to a different worker-attempt generation.
    #[error("stale worker attempt: expected generation {expected}, received {received}")]
    StaleAttempt {
        expected: Counter,
        received: PositiveCounter,
    },
    /// The requested change requires owner/scheduler control or is not a worker edge.
    #[error("worker transition from {from:?} to {to:?} is not allowed")]
    WorkerTransitionForbidden { from: JobState, to: JobState },
    /// Advancing the revision would exceed the supported counter range.
    #[error("job revision counter is exhausted")]
    RevisionExhausted,
    /// Starting or fencing an attempt would exceed the supported counter range.
    #[error("job attempt generation counter is exhausted")]
    AttemptGenerationExhausted,
}

impl JobLifecycle {
    /// Creates a queued lifecycle before any worker attempt has started.
    ///
    /// # Returns
    /// A `JobLifecycle` in `Queued`, with revision one and attempt generation zero.
    pub fn new() -> Self {
        Self {
            state: JobState::Queued,
            revision: PositiveCounter::ONE,
            attempt_generation: Counter::ZERO,
        }
    }

    /// Reads the current state without changing it.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&JobLifecycle` to inspect.
    ///
    /// # Returns
    /// The current `JobState`, copied by value.
    pub fn state(&self) -> JobState {
        self.state
    }

    /// Reads the revision required by the next owner/scheduler control request.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&JobLifecycle` to inspect.
    ///
    /// # Returns
    /// The current `PositiveCounter` revision.
    pub fn revision(&self) -> PositiveCounter {
        self.revision
    }

    /// Reads the current worker-attempt token counter.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&JobLifecycle` to inspect.
    ///
    /// # Returns
    /// A `Counter`, initially zero. Only `Running` and `Verifying` accept worker
    /// reports; a positive generation in another state is not an active token.
    pub fn attempt_generation(&self) -> Counter {
        self.attempt_generation
    }

    /// Applies an owner or scheduler decision against its observed revision.
    ///
    /// # Arguments
    /// - `self`: the mutable `&mut JobLifecycle` being controlled.
    /// - `expected_revision`: the `PositiveCounter` observed by the caller.
    /// - `next`: the requested `JobState`, checked against the design table.
    ///
    /// # Returns
    /// `Ok(())` after the state, revision, and required fencing change together.
    /// `Running -> Verifying` preserves the attempt token; other active-state
    /// exits fence it. Entering `Running` allocates a new token.
    /// Recording `Completed` is reserved for the trusted application after its
    /// independent verification and publication; this policy does not perform them.
    ///
    /// # Errors
    /// Rejects a stale revision, an illegal edge, or an exhausted counter. Every
    /// error leaves all fields unchanged. This method does not stop a process.
    pub fn transition(
        &mut self,
        expected_revision: PositiveCounter,
        next: JobState,
    ) -> Result<(), JobTransitionError> {
        if expected_revision != self.revision {
            return Err(JobTransitionError::RevisionMismatch {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        self.apply_transition(next)
    }

    /// Accepts a result only from the current running or verifying worker attempt.
    ///
    /// # Arguments
    /// - `self`: the mutable `&mut JobLifecycle` receiving the result.
    /// - `generation`: the positive attempt token captured when its worker started.
    /// - `next`: the worker-reported `JobState`.
    ///
    /// # Returns
    /// `Ok(())` after applying an allowed worker result and incrementing revision.
    /// Running workers may request verification, failure, or a waiting state;
    /// verifying workers may fail or wait for storage/input. Completion requires
    /// an owner/scheduler transition after independent verification and publication.
    /// Workers cannot complete, start, requeue, pause, or cancel jobs here.
    ///
    /// # Errors
    /// Rejects inactive or stale attempts, forbidden worker changes, and exhausted
    /// counters without changing any field. Actual output verification and durable
    /// publication must still be performed by the application.
    pub fn worker_transition(
        &mut self,
        generation: PositiveCounter,
        next: JobState,
    ) -> Result<(), JobTransitionError> {
        use JobState::*;
        if !matches!(self.state, Running | Verifying) {
            return Err(JobTransitionError::NoActiveAttempt { state: self.state });
        }
        if generation.value() != self.attempt_generation.value() {
            return Err(JobTransitionError::StaleAttempt {
                expected: self.attempt_generation,
                received: generation,
            });
        }
        let allowed = match self.state {
            Running => matches!(
                next,
                Verifying | Failed | WaitingForSource | WaitingForStorage | WaitingForInput
            ),
            Verifying => matches!(next, Failed | WaitingForStorage | WaitingForInput),
            _ => false,
        };
        if !allowed {
            return Err(JobTransitionError::WorkerTransitionForbidden {
                from: self.state,
                to: next,
            });
        }
        self.apply_transition(next)
    }

    /// Validates and calculates a complete transition before changing any field.
    ///
    /// # Arguments
    /// - `self`: the mutable `&mut JobLifecycle` whose request has passed its
    ///   revision or worker-token check.
    /// - `next`: the proposed `JobState`.
    ///
    /// # Returns
    /// `Ok(())` after atomically updating the in-memory state and counters.
    ///
    /// # Errors
    /// Rejects an illegal edge or a counter limit before applying any change.
    fn apply_transition(&mut self, next: JobState) -> Result<(), JobTransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(JobTransitionError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }
        // Domain counters stop at i64::MAX, so adding one cannot overflow u64.
        let revision = PositiveCounter::from_u64(self.revision.value() + 1)
            .map_err(|_| JobTransitionError::RevisionExhausted)?;
        let fences_attempt = next == JobState::Running
            || self.state == JobState::Verifying
            || (self.state == JobState::Running && next != JobState::Verifying);
        let generation = if fences_attempt {
            Counter::from_u64(self.attempt_generation.value() + 1)
                .map_err(|_| JobTransitionError::AttemptGenerationExhausted)?
        } else {
            self.attempt_generation
        };
        self.state = next;
        self.revision = revision;
        self.attempt_generation = generation;
        Ok(())
    }
}

impl Default for JobLifecycle {
    /// Creates the same fresh queued lifecycle as `JobLifecycle::new`.
    ///
    /// # Returns
    /// A `JobLifecycle` at revision one with no worker attempt allocated.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms revision exhaustion does not partially change state or generation.
    #[test]
    fn revision_exhaustion_leaves_every_field_unchanged() {
        let mut lifecycle = JobLifecycle {
            state: JobState::Queued,
            revision: PositiveCounter::MAX,
            attempt_generation: Counter::ZERO,
        };
        let before = lifecycle.clone();
        assert_eq!(
            lifecycle.transition(lifecycle.revision(), JobState::Running),
            Err(JobTransitionError::RevisionExhausted)
        );
        assert_eq!(lifecycle, before);
    }

    /// Confirms attempt exhaustion does not consume a revision or permit late success.
    #[test]
    fn generation_exhaustion_leaves_owner_and_worker_transitions_unchanged() {
        for (state, next) in [
            (JobState::Queued, JobState::Running),
            (JobState::Running, JobState::Canceled),
            (JobState::Verifying, JobState::Completed),
        ] {
            let mut lifecycle = JobLifecycle {
                state,
                revision: PositiveCounter::ONE,
                attempt_generation: Counter::MAX,
            };
            let before = lifecycle.clone();
            assert_eq!(
                lifecycle.transition(lifecycle.revision(), next),
                Err(JobTransitionError::AttemptGenerationExhausted)
            );
            assert_eq!(lifecycle, before);
            if state == JobState::Verifying {
                assert_eq!(
                    lifecycle.worker_transition(PositiveCounter::MAX, JobState::Failed),
                    Err(JobTransitionError::AttemptGenerationExhausted)
                );
                assert_eq!(lifecycle, before);
            }
        }
    }
}
