//! Tests the pairing policy with explicit monotonic times and trusted peer fixtures.

use std::time::{Duration, Instant};

use jumpstart_domain::{ClientInstanceId, Owner, ServerId};
use jumpstart_protocol::pairing::{PairingError, PairingObservation, PairingWindow};

/// Create one authenticated peer observation for a test source address.
fn peer(source: u8) -> PairingObservation {
    PairingObservation {
        public_key: [7; 32],
        remote: ([192, 168, 0, source], 4000).into(),
        client_nonce: [13; 16],
        instance: ClientInstanceId::new(),
        app: Owner::Photogenic,
        platform: "test-platform".to_owned(),
        name: "test-device".to_owned(),
    }
}

/// Require explicit approval of the exact pending transcript after one token use.
#[test]
fn token_is_consumed_before_exact_approval() {
    let now = Instant::now();
    let (mut window, invitation) = PairingWindow::open(
        now,
        ServerId::new(),
        [1; 32],
        ([192, 168, 0, 1], 8080).into(),
    )
    .unwrap();
    let request = window
        .submit(now, invitation.token, peer(2), |_| false)
        .unwrap();
    let digest = request.transcript;
    let request_id = request.request_id;
    assert!(matches!(
        window.submit(now, invitation.token, peer(3), |_| false),
        Err(PairingError::Consumed)
    ));
    assert!(matches!(
        window.approve(now, blake3::hash(b"another request"), |_| false),
        Err(PairingError::WrongTranscript)
    ));
    assert_eq!(
        window.approve(now, digest, |_| false).unwrap().request_id,
        request_id
    );
    assert!(matches!(
        window.approve(now, digest, |_| false),
        Err(PairingError::WrongTranscript)
    ));
}

/// Reject a key already revoked and recheck revocation before granting a pending key.
#[test]
fn revocation_is_checked_at_submission_and_approval() {
    let now = Instant::now();
    let (mut window, invitation) = PairingWindow::open(
        now,
        ServerId::new(),
        [1; 32],
        ([192, 168, 0, 1], 8080).into(),
    )
    .unwrap();
    assert!(matches!(
        window.submit(now, invitation.token, peer(2), |key| *key == [7; 32]),
        Err(PairingError::Revoked)
    ));
    let digest = window
        .submit(now, invitation.token, peer(2), |_| false)
        .unwrap()
        .transcript;
    assert!(matches!(
        window.approve(now, digest, |key| *key == [7; 32]),
        Err(PairingError::Revoked)
    ));
    assert!(matches!(
        window.approve(now, digest, |_| false),
        Err(PairingError::WrongTranscript)
    ));
}

/// Enforce per-source and total limits, even when attempts come from many sources.
#[test]
fn failed_tokens_are_rate_limited_and_close_the_window() {
    let now = Instant::now();
    let (mut window, invitation) = PairingWindow::open(
        now,
        ServerId::new(),
        [1; 32],
        ([192, 168, 0, 1], 8080).into(),
    )
    .unwrap();
    let mut wrong = invitation.token;
    wrong[0] ^= 1;
    for _ in 0..5 {
        assert!(matches!(
            window.submit(now, wrong, peer(2), |_| false),
            Err(PairingError::InvalidToken)
        ));
    }
    assert!(matches!(
        window.submit(now, invitation.token, peer(2), |_| false),
        Err(PairingError::RateLimited)
    ));
    let later = now + Duration::from_secs(60);
    assert!(matches!(
        window.submit(later, wrong, peer(2), |_| false),
        Err(PairingError::InvalidToken)
    ));
    for source in 3..17 {
        assert!(matches!(
            window.submit(later, wrong, peer(source), |_| false),
            Err(PairingError::InvalidToken)
        ));
    }
    assert!(matches!(
        window.submit(later, invitation.token, peer(17), |_| false),
        Err(PairingError::Closed)
    ));
}

/// A decision at the five-minute boundary cannot revive a pending request.
#[test]
fn pending_request_expires_with_its_window() {
    let now = Instant::now();
    let (mut window, invitation) = PairingWindow::open(
        now,
        ServerId::new(),
        [1; 32],
        ([192, 168, 0, 1], 8080).into(),
    )
    .unwrap();
    let digest = window
        .submit(now, invitation.token, peer(2), |_| false)
        .unwrap()
        .transcript;
    assert!(matches!(
        window.approve(now + Duration::from_secs(300), digest, |_| false),
        Err(PairingError::Closed)
    ));
}
