//! Decides whether failed work may try again automatically, and how long to wait.
//!
//! For example, a temporarily unavailable music provider may recover, but retrying
//! forever would waste work. A sign-in failure instead needs the user's input.
//! This policy limits one attempt group to six automatic retries within 15 minutes
//! and respects a provider's requested minimum wait. An explicit user retry needs
//! a new policy with a fresh elapsed-time origin.
//!
//! The caller supplies elapsed time and a random delay; this module reads no clock,
//! draws no randomness, sleeps nowhere, and starts no worker. A future scheduler
//! must arrange the returned wait and must not hold a worker while waiting.

use std::time::Duration;

use thiserror::Error;

/// Maximum number of automatic retries after the initial attempt in one group.
pub const MAX_AUTOMATIC_RETRIES: u8 = 6;

/// Elapsed-time limit covering initial work, retry work, and all intervening waits.
pub const AUTOMATIC_RETRY_BUDGET: Duration = Duration::from_secs(15 * 60);

/// Inclusive full-jitter ceilings for the first through sixth automatic retries.
const RETRY_CEILINGS: [Duration; MAX_AUTOMATIC_RETRIES as usize] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
    Duration::from_secs(60),
];

/// Describes the cause of a failed attempt without treating failure as empty success.
///
/// Only `Transient` permits automatic retries. Classifying a particular provider's
/// error or deciding how the user can fix it belongs to the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    /// A temporary failure, such as a provider that is momentarily unavailable.
    Transient,
    /// The source no longer matches the identity accepted for this work.
    SourceChanged,
    /// Credentials, quota, permission, review, or another user action is needed.
    InputRequired,
    /// The requested operation or source capability is unavailable.
    Unsupported,
    /// Output or source data failed validation and needs a separate recovery choice.
    Corrupt,
    /// Repeating the same operation cannot resolve the failure.
    Permanent,
}

/// Explains why this attempt group cannot schedule another automatic retry.
///
/// A stop remains in force for this policy. Explicit user retry requires a new
/// policy rather than changing this reason or reclaiming an earlier retry slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryStop {
    /// The failure class requires input, source validation, or a terminal outcome.
    NonTransient(FailureClass),
    /// All six automatic retry slots have already been scheduled.
    RetryLimitExhausted,
    /// The elapsed time or requested wait leaves no retry start before the deadline.
    AutomaticBudgetExhausted,
}

/// A scheduling decision only; no attempt, timer, or job-state transition is executed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    /// Arrange a wait without retaining active worker resources, then retry.
    Wait {
        /// Minimum `Duration` to wait after the supplied elapsed-time observation.
        delay: Duration,
        /// Scheduled automatic retry number in `1..=MAX_AUTOMATIC_RETRIES`.
        retry_number: u8,
    },
    /// Require the caller to record a failed or input-required outcome.
    Stop(RetryStop),
}

/// Identifies invalid policy inputs; these errors leave the policy unchanged.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RetryPolicyError {
    /// Elapsed time moved backwards relative to the last accepted observation.
    #[error("retry elapsed time moved backwards from {previous:?} to {provided:?}")]
    ElapsedTimeWentBackwards {
        /// Last accepted elapsed `Duration` since this group's start.
        previous: Duration,
        /// Smaller elapsed `Duration` supplied by the caller.
        provided: Duration,
    },
    /// A proposed full-jitter sample exceeded this retry's inclusive ceiling.
    #[error("retry jitter {provided:?} exceeds the inclusive ceiling {ceiling:?}")]
    InvalidJitter {
        /// Candidate random `Duration` supplied by the caller.
        provided: Duration,
        /// Largest sample permitted for this retry number.
        ceiling: Duration,
    },
}

/// Tracks the bounded automatic retry allowance for one attempt group.
///
/// Start measuring elapsed time before the initial attempt. Supply time since that
/// same origin after each failed attempt, including time spent working or waiting.
/// An accepted `Wait` consumes one retry slot immediately; the caller must honor
/// its minimum delay before the next attempt. Invalid inputs consume nothing.
///
/// Retry starts must fall strictly before the 15-minute deadline: a wait ending
/// exactly at the deadline is exhausted, as is a failure observed at the deadline.
/// This policy does not bound an already-running attempt or implement a timeout.
/// A stop is latched, so a later shorter provider hint cannot revive automatic work.
/// Private state has no deserializer or reset method; persistence is not implemented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Number of accepted automatic wait decisions, always at most six.
    scheduled_retries: u8,
    /// Most recent elapsed time accepted by an evaluation, initially zero.
    last_elapsed: Duration,
    /// First stop reason, retained until this policy is discarded.
    stopped: Option<RetryStop>,
}

impl RetryPolicy {
    /// Starts an unused budget for the initial attempt and its automatic retries.
    ///
    /// # Returns
    /// A `RetryPolicy` with zero scheduled retries and no stop reason. The caller
    /// establishes its elapsed-time origin when starting this attempt group.
    pub const fn new() -> Self {
        Self {
            scheduled_retries: 0,
            last_elapsed: Duration::ZERO,
            stopped: None,
        }
    }

    /// Exposes the inclusive ceiling from which the caller draws full jitter.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&RetryPolicy` for the current attempt group.
    ///
    /// # Returns
    /// `Some(Duration)` for the next unused retry slot, or `None` after six slots
    /// or a stop. Elapsed-time eligibility is checked separately by `evaluate`.
    pub fn next_ceiling(&self) -> Option<Duration> {
        if self.stopped.is_some() {
            return None;
        }
        RETRY_CEILINGS
            .get(usize::from(self.scheduled_retries))
            .copied()
    }

    /// Reports how many automatic retry slots have been consumed by wait decisions.
    ///
    /// # Arguments
    /// - `self`: the borrowed `&RetryPolicy` whose allowance is being inspected.
    ///
    /// # Returns
    /// A `u8` in `0..=MAX_AUTOMATIC_RETRIES`; stopped or rejected decisions do not
    /// increment it, and cancellation does not return a previously scheduled slot.
    pub const fn scheduled_retries(&self) -> u8 {
        self.scheduled_retries
    }

    /// Records a failure and returns a bounded wait or a latched stop decision.
    ///
    /// # Arguments
    /// - `self`: the mutable `&mut RetryPolicy` for this attempt group.
    /// - `failure`: the `FailureClass` determined by the caller's error handling.
    /// - `elapsed`: a nondecreasing `Duration` since the group's initial start.
    /// - `jitter`: a caller-drawn `Duration` uniformly sampled from zero through
    ///   `next_ceiling()`, inclusive. This policy validates its range, not its
    ///   randomness. It is unused for nontransient failures or after a stop/limit.
    /// - `retry_after`: an optional provider minimum wait as `Option<Duration>`
    ///   relative to this observation, after the caller parses any provider header.
    ///
    /// # Returns
    /// `Result<RetryDecision, RetryPolicyError>`. For an eligible transient failure,
    /// the delay is the larger of `jitter` and `retry_after`; it is never shortened
    /// to fit the remaining budget. A requested wait ending at or beyond the
    /// deadline instead stops the group. A previously stopped group keeps its
    /// first stop reason. The caller still owns timers, failures, and user prompts.
    ///
    /// # Errors
    /// Returns `ElapsedTimeWentBackwards` for an older observation or `InvalidJitter`
    /// for an out-of-range sample when a transient retry slot is available. Neither
    /// error changes state. `Duration` comparisons avoid adding untrusted delays,
    /// so even maximum durations cannot overflow an elapsed-plus-wait calculation.
    pub fn evaluate(
        &mut self,
        failure: FailureClass,
        elapsed: Duration,
        jitter: Duration,
        retry_after: Option<Duration>,
    ) -> Result<RetryDecision, RetryPolicyError> {
        if elapsed < self.last_elapsed {
            return Err(RetryPolicyError::ElapsedTimeWentBackwards {
                previous: self.last_elapsed,
                provided: elapsed,
            });
        }

        if let Some(reason) = self.stopped {
            self.last_elapsed = elapsed;
            return Ok(RetryDecision::Stop(reason));
        }

        if failure != FailureClass::Transient {
            return Ok(self.stop(RetryStop::NonTransient(failure), elapsed));
        }

        let Some(ceiling) = self.next_ceiling() else {
            return Ok(self.stop(RetryStop::RetryLimitExhausted, elapsed));
        };
        if jitter > ceiling {
            return Err(RetryPolicyError::InvalidJitter {
                provided: jitter,
                ceiling,
            });
        }

        let delay = jitter.max(retry_after.unwrap_or(Duration::ZERO));
        let remaining = AUTOMATIC_RETRY_BUDGET.saturating_sub(elapsed);
        if elapsed >= AUTOMATIC_RETRY_BUDGET || delay >= remaining {
            return Ok(self.stop(RetryStop::AutomaticBudgetExhausted, elapsed));
        }

        self.scheduled_retries += 1;
        self.last_elapsed = elapsed;
        Ok(RetryDecision::Wait {
            delay,
            retry_number: self.scheduled_retries,
        })
    }

    /// Retains the first stop reason and accepts its elapsed-time observation.
    ///
    /// # Arguments
    /// - `self`: the mutable `&mut RetryPolicy`, known not to have stopped yet.
    /// - `reason`: the `RetryStop` that prevents further automatic work.
    /// - `elapsed`: the already-validated `Duration` for this failure observation.
    ///
    /// # Returns
    /// The `RetryDecision::Stop` carrying the retained reason, with no slot consumed.
    fn stop(&mut self, reason: RetryStop, elapsed: Duration) -> RetryDecision {
        self.stopped = Some(reason);
        self.last_elapsed = elapsed;
        RetryDecision::Stop(reason)
    }
}

impl Default for RetryPolicy {
    /// Starts a fresh attempt group using the same bounded allowance as `new`.
    ///
    /// # Returns
    /// An unused `RetryPolicy`; this does not reset any existing attempt group.
    fn default() -> Self {
        Self::new()
    }
}
