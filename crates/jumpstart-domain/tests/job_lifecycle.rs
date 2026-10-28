//! Tests the rules for controlling a download, conversion, or other background job.
//!
//! For example, canceling a video conversion must make its old worker's later
//! success report harmless. These tests cover the exact state table, stale control
//! revisions, numbered worker-attempt tokens, and terminal jobs that need successors.
//! They run entirely in memory and do not launch workers or verify actual files.

use jumpstart_domain::{Counter, JobLifecycle, JobState, JobTransitionError, PositiveCounter};
use serde_json::json;

const STATES: [JobState; 10] = [
    JobState::Queued,
    JobState::Running,
    JobState::Paused,
    JobState::WaitingForSource,
    JobState::WaitingForStorage,
    JobState::WaitingForInput,
    JobState::Verifying,
    JobState::Completed,
    JobState::Failed,
    JobState::Canceled,
];

/// Builds a reachable lifecycle through public owner/scheduler transitions.
///
/// # Arguments
/// - `state`: the target `JobState` for a test fixture.
///
/// # Returns
/// A `JobLifecycle` in the requested state with naturally advanced counters.
///
/// # Panics
/// Panics if a transition required by the design unexpectedly fails.
fn in_state(state: JobState) -> JobLifecycle {
    use JobState::*;
    let path: &[JobState] = match state {
        Queued => &[],
        Running => &[Running],
        Paused => &[Running, Paused],
        WaitingForSource => &[WaitingForSource],
        WaitingForStorage => &[WaitingForStorage],
        WaitingForInput => &[WaitingForInput],
        Verifying => &[Running, Verifying],
        Completed => &[Running, Verifying, Completed],
        Failed => &[Running, Failed],
        Canceled => &[Canceled],
    };
    let mut job = JobLifecycle::new();
    for next in path {
        job.transition(job.revision(), *next)
            .expect("reachable fixture transition");
    }
    job
}

/// Captures the positive token of an active fixture worker.
///
/// # Arguments
/// - `job`: a borrowed `&JobLifecycle` in an active state.
///
/// # Returns
/// Its attempt generation as a `PositiveCounter` suitable for worker reports.
///
/// # Panics
/// Panics if the fixture has not started a worker.
fn token(job: &JobLifecycle) -> PositiveCounter {
    PositiveCounter::from_u64(job.attempt_generation().value()).expect("active fixture token")
}

/// Compares all 100 possible state pairs against the independent design edge list,
/// including self-transitions, terminal states, and error non-mutation.
#[test]
fn exact_design_matrix_controls_owner_transitions() {
    use JobState::*;
    let allowed = [
        (Queued, Running),
        (Queued, WaitingForSource),
        (Queued, WaitingForStorage),
        (Queued, WaitingForInput),
        (Queued, Canceled),
        (Running, Paused),
        (Running, WaitingForSource),
        (Running, WaitingForStorage),
        (Running, WaitingForInput),
        (Running, Verifying),
        (Running, Failed),
        (Running, Canceled),
        (Paused, Queued),
        (Paused, Canceled),
        (WaitingForSource, Queued),
        (WaitingForSource, WaitingForInput),
        (WaitingForSource, Failed),
        (WaitingForSource, Canceled),
        (WaitingForStorage, Queued),
        (WaitingForStorage, WaitingForInput),
        (WaitingForStorage, Failed),
        (WaitingForStorage, Canceled),
        (WaitingForInput, Queued),
        (WaitingForInput, Failed),
        (WaitingForInput, Canceled),
        (Verifying, Completed),
        (Verifying, WaitingForStorage),
        (Verifying, WaitingForInput),
        (Verifying, Failed),
        (Verifying, Canceled),
    ];
    for from in STATES {
        for to in STATES {
            let expected = allowed.contains(&(from, to));
            assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            let mut job = in_state(from);
            let before = job.clone();
            let result = job.transition(job.revision(), to);
            assert_eq!(result.is_ok(), expected, "{from:?} -> {to:?}: {result:?}");
            if expected {
                assert_eq!(job.state(), to);
                assert_eq!(job.revision().value(), before.revision().value() + 1);
            } else {
                assert_eq!(job, before);
            }
        }
    }
}

/// Checks fresh state, stable snake-case wire names, unknown-state rejection,
/// and the three terminal states requiring a new job.
#[test]
fn initial_values_and_state_wire_names_are_explicit() {
    let fresh = JobLifecycle::new();
    assert_eq!(fresh, JobLifecycle::default());
    assert_eq!(fresh.state(), JobState::Queued);
    assert_eq!(fresh.revision(), PositiveCounter::ONE);
    assert_eq!(fresh.attempt_generation(), Counter::ZERO);
    let names = [
        "queued",
        "running",
        "paused",
        "waiting_for_source",
        "waiting_for_storage",
        "waiting_for_input",
        "verifying",
        "completed",
        "failed",
        "canceled",
    ];
    for (state, name) in STATES.into_iter().zip(names) {
        assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
        assert_eq!(
            serde_json::from_value::<JobState>(json!(name)).unwrap(),
            state
        );
        assert_eq!(
            state.is_terminal(),
            matches!(
                state,
                JobState::Completed | JobState::Failed | JobState::Canceled
            )
        );
    }
    for invalid in [
        json!("Queued"),
        json!("waiting for source"),
        json!("finished"),
        json!(null),
    ] {
        assert!(serde_json::from_value::<JobState>(invalid).is_err());
    }
}

/// Ensures a worker update makes older owner controls stale, while keeping the
/// current worker token valid through the Running-to-Verifying handoff.
#[test]
fn stale_owner_revision_cannot_overwrite_a_newer_worker_result() {
    let mut job = in_state(JobState::Running);
    let observed = job.revision();
    let worker = token(&job);
    job.worker_transition(worker, JobState::Verifying)
        .expect("verification handoff");
    assert_eq!(token(&job), worker);
    let before = job.clone();
    assert!(matches!(
        job.transition(observed, JobState::Canceled),
        Err(JobTransitionError::RevisionMismatch { .. })
    ));
    assert_eq!(job, before);
    job.transition(job.revision(), JobState::Canceled)
        .expect("current owner may cancel");
}

/// Checks that pausing fences a worker and resuming through Queued starts a new
/// generation, so old output cannot affect the replacement attempt.
#[test]
fn resumed_job_rejects_the_previous_attempt_generation() {
    let mut job = in_state(JobState::Running);
    let old_worker = token(&job);
    job.transition(job.revision(), JobState::Paused).unwrap();
    assert_eq!(job.attempt_generation().value(), old_worker.value() + 1);
    job.transition(job.revision(), JobState::Queued).unwrap();
    job.transition(job.revision(), JobState::Running).unwrap();
    assert_eq!(job.attempt_generation().value(), old_worker.value() + 2);
    let before = job.clone();
    assert!(matches!(
        job.worker_transition(old_worker, JobState::Verifying),
        Err(JobTransitionError::StaleAttempt { .. })
    ));
    assert_eq!(job, before);
    let current_worker = token(&job);
    job.worker_transition(current_worker, JobState::Verifying)
        .unwrap();
    job.transition(job.revision(), JobState::Completed).unwrap();
    assert_eq!(job.state(), JobState::Completed);
    assert_eq!(job.attempt_generation().value(), current_worker.value() + 1);
}

/// Proves cancellation wins over late output from both running and verifying
/// workers, leaving the recorded cancellation and counters unchanged.
#[test]
fn cancellation_fences_late_worker_completion() {
    for state in [JobState::Running, JobState::Verifying] {
        let mut job = in_state(state);
        let old_worker = token(&job);
        job.transition(job.revision(), JobState::Canceled)
            .expect("cancel current attempt");
        assert_eq!(job.attempt_generation().value(), old_worker.value() + 1);
        let canceled = job.clone();
        for late_result in [JobState::Verifying, JobState::Completed, JobState::Failed] {
            assert!(job.worker_transition(old_worker, late_result).is_err());
            assert_eq!(job, canceled);
        }
    }
}

/// Reserves successful completion for the daemon's control path after its own
/// verification/publication decision, rather than trusting a worker's claim.
#[test]
fn completion_requires_an_owner_decision_from_verifying() {
    let mut job = in_state(JobState::Running);
    let worker = token(&job);
    job.worker_transition(worker, JobState::Verifying)
        .expect("worker may report output ready for verification");
    let before = job.clone();
    assert!(matches!(
        job.worker_transition(worker, JobState::Completed),
        Err(JobTransitionError::WorkerTransitionForbidden { .. })
    ));
    assert_eq!(job, before);
    // Application code must perform verification and publication before using
    // this trusted control path; the pure policy does not inspect actual files.
    job.transition(job.revision(), JobState::Completed)
        .expect("owner may record its verified publication outcome");
    assert_eq!(job.state(), JobState::Completed);
    assert_eq!(job.attempt_generation().value(), worker.value() + 1);
}

/// Checks that workers cannot issue owner controls or skip verification, and
/// cannot report results while a job is queued, paused, waiting, or terminal.
#[test]
fn workers_have_only_the_explicit_active_attempt_edges() {
    for (state, next) in [
        (JobState::Running, JobState::Paused),
        (JobState::Running, JobState::Canceled),
        (JobState::Running, JobState::Completed),
        (JobState::Verifying, JobState::Completed),
        (JobState::Verifying, JobState::Canceled),
        (JobState::Verifying, JobState::WaitingForSource),
    ] {
        let mut job = in_state(state);
        let worker = token(&job);
        let before = job.clone();
        assert!(matches!(
            job.worker_transition(worker, next),
            Err(JobTransitionError::WorkerTransitionForbidden { .. })
        ));
        assert_eq!(job, before);
    }
    for state in STATES
        .into_iter()
        .filter(|state| !matches!(state, JobState::Running | JobState::Verifying))
    {
        let mut job = in_state(state);
        let before = job.clone();
        assert!(matches!(
            job.worker_transition(PositiveCounter::ONE, JobState::Running),
            Err(JobTransitionError::NoActiveAttempt { .. })
        ));
        assert_eq!(job, before);
    }
}

/// Confirms leaving active work for a wait fences its token and that terminal
/// jobs can only be retried by creating a fresh lifecycle.
#[test]
fn waiting_and_terminal_outcomes_do_not_reopen_old_attempts() {
    for (state, wait) in [
        (JobState::Running, JobState::WaitingForSource),
        (JobState::Running, JobState::WaitingForStorage),
        (JobState::Running, JobState::WaitingForInput),
        (JobState::Verifying, JobState::WaitingForStorage),
        (JobState::Verifying, JobState::WaitingForInput),
    ] {
        let mut job = in_state(state);
        let worker = token(&job);
        job.worker_transition(worker, wait)
            .expect("worker may explain why it must wait");
        assert_eq!(job.attempt_generation().value(), worker.value() + 1);
        let before = job.clone();
        assert!(job.worker_transition(worker, JobState::Completed).is_err());
        assert_eq!(job, before);
    }
    for terminal in [JobState::Completed, JobState::Failed, JobState::Canceled] {
        let mut job = in_state(terminal);
        let before = job.clone();
        assert!(job.transition(job.revision(), JobState::Queued).is_err());
        assert_eq!(job, before);
        assert_eq!(JobLifecycle::new().state(), JobState::Queued);
    }
}
