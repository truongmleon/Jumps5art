//! Checks the retry decisions that prevent temporary failures from looping forever.
//!
//! A caller supplies elapsed time and a proposed random delay, so these examples
//! need no clock, sleeps, provider connection, or running worker. They check the
//! six retry ceilings, the 15-minute deadline, and provider-requested minimum waits.
//! They also prove that sign-in and other nontransient failures stop immediately,
//! invalid inputs preserve the allowance, and stopped work needs a new policy.

use std::time::Duration;

use jumpstart_domain::{
    AUTOMATIC_RETRY_BUDGET, FailureClass, MAX_AUTOMATIC_RETRIES, RetryDecision, RetryPolicy,
    RetryPolicyError, RetryStop,
};

/// Checks the complete ceiling schedule and rejects a seventh automatic retry,
/// even though the elapsed-time budget still has room.
#[test]
fn schedules_exactly_six_retries_with_the_declared_inclusive_ceilings() {
    let mut policy = RetryPolicy::new();
    let mut elapsed = Duration::ZERO;

    for (index, seconds) in [2, 4, 8, 16, 32, 60].into_iter().enumerate() {
        let ceiling = Duration::from_secs(seconds);
        assert_eq!(policy.next_ceiling(), Some(ceiling));
        assert_eq!(
            policy.evaluate(FailureClass::Transient, elapsed, ceiling, None),
            Ok(RetryDecision::Wait {
                delay: ceiling,
                retry_number: u8::try_from(index + 1).unwrap(),
            })
        );
        elapsed += ceiling;
    }

    assert!(elapsed < AUTOMATIC_RETRY_BUDGET);
    assert_eq!(policy.scheduled_retries(), MAX_AUTOMATIC_RETRIES);
    assert_eq!(policy.next_ceiling(), None);
    assert_eq!(
        policy.evaluate(FailureClass::Transient, elapsed, Duration::ZERO, None),
        Ok(RetryDecision::Stop(RetryStop::RetryLimitExhausted))
    );
    assert_eq!(policy.scheduled_retries(), MAX_AUTOMATIC_RETRIES);
}

/// Proves that zero belongs to every full-jitter range without bypassing the
/// retry-count bound when failures happen at the same elapsed-time observation.
#[test]
fn zero_jitter_is_valid_but_still_consumes_each_retry_slot() {
    let mut policy = RetryPolicy::new();

    for retry_number in 1..=MAX_AUTOMATIC_RETRIES {
        assert_eq!(
            policy.evaluate(
                FailureClass::Transient,
                Duration::ZERO,
                Duration::ZERO,
                None,
            ),
            Ok(RetryDecision::Wait {
                delay: Duration::ZERO,
                retry_number,
            })
        );
    }

    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::ZERO,
            Duration::ZERO,
            None,
        ),
        Ok(RetryDecision::Stop(RetryStop::RetryLimitExhausted))
    );
}

/// Keeps a provider's longer minimum wait even beyond the jitter ceiling, while
/// a shorter provider minimum never replaces a larger valid jitter sample.
#[test]
fn respects_provider_minimum_without_shortening_either_wait() {
    let mut policy = RetryPolicy::new();
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(10),
            Duration::from_secs(2),
            Some(Duration::from_secs(180)),
        ),
        Ok(RetryDecision::Wait {
            delay: Duration::from_secs(180),
            retry_number: 1,
        })
    );
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(190),
            Duration::from_secs(4),
            Some(Duration::from_secs(1)),
        ),
        Ok(RetryDecision::Wait {
            delay: Duration::from_secs(4),
            retry_number: 2,
        })
    );
}

/// Refuses a provider wait that exceeds the remaining budget instead of clipping
/// the wait, and prevents a later shorter hint from reviving the same group.
#[test]
fn excessive_provider_wait_exhausts_and_latches_the_automatic_budget() {
    let mut policy = RetryPolicy::new();
    let stopped = RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted);
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(10),
            Duration::ZERO,
            Some(Duration::from_secs(891)),
        ),
        Ok(stopped)
    );
    assert_eq!(policy.scheduled_retries(), 0);
    assert_eq!(policy.next_ceiling(), None);
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(11),
            Duration::ZERO,
            None,
        ),
        Ok(stopped)
    );
    assert_eq!(policy.scheduled_retries(), 0);
}

/// Includes time spent on the initial attempt rather than counting only previous
/// backoff waits toward the automatic retry deadline.
#[test]
fn time_spent_working_counts_toward_the_same_elapsed_budget() {
    let mut policy = RetryPolicy::new();
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(899),
            Duration::from_secs(2),
            None,
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );
    assert_eq!(policy.scheduled_retries(), 0);
}

/// Allows a retry just before the deadline but rejects a wait ending exactly at
/// it, as well as a failure observed when the deadline has already arrived.
#[test]
fn retry_starts_must_be_strictly_before_the_deadline() {
    let mut before_deadline = RetryPolicy::new();
    let almost_one_second = Duration::from_nanos(999_999_999);
    assert_eq!(
        before_deadline.evaluate(
            FailureClass::Transient,
            Duration::from_secs(899),
            almost_one_second,
            None,
        ),
        Ok(RetryDecision::Wait {
            delay: almost_one_second,
            retry_number: 1,
        })
    );

    let mut ends_at_deadline = RetryPolicy::new();
    assert_eq!(
        ends_at_deadline.evaluate(
            FailureClass::Transient,
            Duration::from_secs(899),
            Duration::from_secs(1),
            None,
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );
    assert_eq!(ends_at_deadline.scheduled_retries(), 0);

    let mut already_at_deadline = RetryPolicy::new();
    assert_eq!(
        already_at_deadline.evaluate(
            FailureClass::Transient,
            AUTOMATIC_RETRY_BUDGET,
            Duration::ZERO,
            None,
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );
}

/// Applies the same deadline check to a provider-requested minimum, including
/// nanosecond precision immediately before the remaining budget is consumed.
#[test]
fn provider_wait_at_the_remaining_budget_requires_manual_retry() {
    let mut fits = RetryPolicy::new();
    let almost_full_budget = AUTOMATIC_RETRY_BUDGET - Duration::from_nanos(1);
    assert_eq!(
        fits.evaluate(
            FailureClass::Transient,
            Duration::ZERO,
            Duration::ZERO,
            Some(almost_full_budget),
        ),
        Ok(RetryDecision::Wait {
            delay: almost_full_budget,
            retry_number: 1,
        })
    );

    let mut ends_at_deadline = RetryPolicy::new();
    assert_eq!(
        ends_at_deadline.evaluate(
            FailureClass::Transient,
            Duration::ZERO,
            Duration::ZERO,
            Some(AUTOMATIC_RETRY_BUDGET),
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );
}

/// Stops every nontransient class immediately, ignoring unused delay inputs and
/// retaining the original classification on subsequent evaluations.
#[test]
fn only_transient_failures_can_schedule_automatic_retries() {
    for failure in [
        FailureClass::SourceChanged,
        FailureClass::InputRequired,
        FailureClass::Unsupported,
        FailureClass::Corrupt,
        FailureClass::Permanent,
    ] {
        let mut policy = RetryPolicy::new();
        let stopped = RetryDecision::Stop(RetryStop::NonTransient(failure));
        assert_eq!(
            policy.evaluate(
                failure,
                Duration::from_secs(1),
                Duration::MAX,
                Some(Duration::MAX),
            ),
            Ok(stopped)
        );
        assert_eq!(policy.scheduled_retries(), 0);
        assert_eq!(policy.next_ceiling(), None);
        assert_eq!(
            policy.evaluate(
                FailureClass::Transient,
                Duration::from_secs(2),
                Duration::ZERO,
                None,
            ),
            Ok(stopped)
        );
    }
}

/// Rejects a sample one nanosecond beyond the next ceiling without consuming a
/// retry slot or accepting the invalid call's later elapsed-time observation.
#[test]
fn invalid_jitter_leaves_all_policy_state_unchanged() {
    let mut policy = RetryPolicy::new();
    policy
        .evaluate(
            FailureClass::Transient,
            Duration::from_secs(10),
            Duration::from_secs(1),
            None,
        )
        .unwrap();
    let before = policy.clone();
    let invalid_sample = Duration::from_secs(4) + Duration::from_nanos(1);
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(12),
            invalid_sample,
            Some(Duration::MAX),
        ),
        Err(RetryPolicyError::InvalidJitter {
            provided: invalid_sample,
            ceiling: Duration::from_secs(4),
        })
    );
    assert_eq!(policy, before);
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(11),
            Duration::from_secs(4),
            None,
        ),
        Ok(RetryDecision::Wait {
            delay: Duration::from_secs(4),
            retry_number: 2,
        })
    );
}

/// Refuses backwards elapsed time without recording the new failure or changing
/// the previously consumed retry allowance.
#[test]
fn backwards_elapsed_time_is_rejected_without_mutation() {
    let mut policy = RetryPolicy::new();
    policy
        .evaluate(
            FailureClass::Transient,
            Duration::from_secs(10),
            Duration::ZERO,
            None,
        )
        .unwrap();
    let before = policy.clone();
    assert_eq!(
        policy.evaluate(
            FailureClass::InputRequired,
            Duration::from_secs(9),
            Duration::ZERO,
            None,
        ),
        Err(RetryPolicyError::ElapsedTimeWentBackwards {
            previous: Duration::from_secs(10),
            provided: Duration::from_secs(9),
        })
    );
    assert_eq!(policy, before);
    assert_eq!(policy.next_ceiling(), Some(Duration::from_secs(4)));
}

/// Retains monotonic-time validation after a group stops, while allowing later
/// observations to report that same stop reason.
#[test]
fn stopped_groups_still_reject_backwards_elapsed_time() {
    let mut policy = RetryPolicy::new();
    let stopped = RetryDecision::Stop(RetryStop::NonTransient(FailureClass::InputRequired));
    assert_eq!(
        policy.evaluate(
            FailureClass::InputRequired,
            Duration::from_secs(20),
            Duration::ZERO,
            None,
        ),
        Ok(stopped)
    );
    let before = policy.clone();
    assert!(matches!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(19),
            Duration::ZERO,
            None,
        ),
        Err(RetryPolicyError::ElapsedTimeWentBackwards { .. })
    ));
    assert_eq!(policy, before);
    assert_eq!(
        policy.evaluate(
            FailureClass::Transient,
            Duration::from_secs(21),
            Duration::ZERO,
            None,
        ),
        Ok(stopped)
    );
}

/// Starts a fresh count and elapsed-time origin only by constructing another
/// policy, leaving the exhausted attempt group stopped.
#[test]
fn explicit_user_retry_uses_a_new_policy() {
    let mut exhausted = RetryPolicy::new();
    assert_eq!(
        exhausted.evaluate(
            FailureClass::Transient,
            AUTOMATIC_RETRY_BUDGET,
            Duration::ZERO,
            None,
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );

    let mut fresh = RetryPolicy::new();
    assert_eq!(fresh, RetryPolicy::default());
    assert_eq!(
        fresh.evaluate(
            FailureClass::Transient,
            Duration::ZERO,
            Duration::ZERO,
            None,
        ),
        Ok(RetryDecision::Wait {
            delay: Duration::ZERO,
            retry_number: 1,
        })
    );
    assert_eq!(exhausted.next_ceiling(), None);
    assert_eq!(exhausted.scheduled_retries(), 0);
}

/// Handles maximum representable elapsed/provider durations using comparisons,
/// and rejects a maximum jitter sample without arithmetic overflow or mutation.
#[test]
fn maximum_duration_inputs_cannot_overflow_retry_calculations() {
    let mut huge_elapsed = RetryPolicy::new();
    assert_eq!(
        huge_elapsed.evaluate(
            FailureClass::Transient,
            Duration::MAX,
            Duration::ZERO,
            Some(Duration::MAX),
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );

    let mut huge_provider_wait = RetryPolicy::new();
    assert_eq!(
        huge_provider_wait.evaluate(
            FailureClass::Transient,
            Duration::from_secs(899),
            Duration::ZERO,
            Some(Duration::MAX),
        ),
        Ok(RetryDecision::Stop(RetryStop::AutomaticBudgetExhausted))
    );

    let mut huge_sample = RetryPolicy::new();
    assert_eq!(
        huge_sample.evaluate(FailureClass::Transient, Duration::ZERO, Duration::MAX, None,),
        Err(RetryPolicyError::InvalidJitter {
            provided: Duration::MAX,
            ceiling: Duration::from_secs(2),
        })
    );
    assert_eq!(huge_sample, RetryPolicy::new());
}
