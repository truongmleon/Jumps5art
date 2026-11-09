//! Tests Worlds adapter confirmations without requiring a running Java server.
//!
//! Fixtures exercise the wire codec and prove that stale responses or unsafe
//! observed state cannot establish readiness, a fixed capture, or resumed saving.

use jumpstart_domain::{BlobId, JobId, PositiveCounter};
use jumpstart_protocol::{
    read_frame,
    worlds_control::{
        WorldsControlCommand as Command, WorldsControlError as Error,
        WorldsControlOutcome as Outcome, WorldsControlReply, WorldsControlRequest, WorldsExchange,
        WorldsObservedState,
    },
    write_frame,
};

/// Create one checked health request and an adapter's matching startup reply.
fn startup() -> (WorldsControlRequest, WorldsControlReply) {
    let exchange = WorldsExchange {
        operation_id: JobId::new(),
        generation: PositiveCounter::ONE,
        command_seq: PositiveCounter::ONE,
    };
    let request = WorldsControlRequest {
        protocol_version: PositiveCounter::ONE,
        exchange,
        command: Command::Health,
    };
    let reply = WorldsControlReply {
        protocol_version: request.protocol_version,
        exchange,
        state: WorldsObservedState {
            world_loaded: true,
            saving_enabled: true,
            admission_open: false,
        },
        outcome: Outcome::Health,
    };
    (request, reply)
}

/// Verify real frame encoding and check readiness after decoding the exact reply.
#[test]
fn framed_startup_confirmation_requires_safe_state() {
    let (request, reply) = startup();
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &request).unwrap();
    write_frame(&mut bytes, &reply).unwrap();
    let mut stream = bytes.as_slice();
    let request: WorldsControlRequest = read_frame(&mut stream).unwrap();
    let mut reply: WorldsControlReply = read_frame(&mut stream).unwrap();
    assert!(stream.is_empty());
    assert_eq!(reply.check_startup_health(&request), Ok(()));
    reply.state.saving_enabled = false;
    assert_eq!(
        reply.check_startup_health(&request),
        Err(Error::UnsafeState)
    );
    reply.state.saving_enabled = true;
    reply.state.admission_open = true;
    assert_eq!(
        reply.check_startup_health(&request),
        Err(Error::UnsafeState)
    );
}

/// A matching action cannot make an old command, process attempt, or version current.
#[test]
fn rejects_substituted_protocol_operation_generation_and_sequence() {
    let (request, reply) = startup();
    let two = PositiveCounter::from_u64(2).unwrap();
    let wrong_version = WorldsControlReply {
        protocol_version: two,
        ..reply
    };
    assert_eq!(
        wrong_version.checked_state(&request),
        Err(Error::ProtocolMismatch)
    );
    for exchange in [
        WorldsExchange {
            generation: two,
            ..reply.exchange
        },
        WorldsExchange {
            command_seq: two,
            ..reply.exchange
        },
        WorldsExchange {
            operation_id: JobId::new(),
            ..reply.exchange
        },
    ] {
        let stale = WorldsControlReply { exchange, ..reply };
        assert_eq!(stale.checked_state(&request), Err(Error::WrongExchange));
    }
}

/// Match both the fixed source's digest and the adapter's observed freeze state.
#[test]
fn capture_and_resume_cannot_accept_an_incompatible_state() {
    let (mut request, mut reply) = startup();
    let manifest_hash = BlobId::hash_bytes(b"fixed world files");
    request.command = Command::CompleteCapture { manifest_hash };
    reply.outcome = Outcome::CaptureCompleted { manifest_hash };
    assert_eq!(reply.checked_state(&request), Err(Error::UnsafeState));
    reply.state.saving_enabled = false;
    assert!(reply.checked_state(&request).is_ok());
    reply.outcome = Outcome::CaptureCompleted {
        manifest_hash: BlobId::hash_bytes(b"another source"),
    };
    assert_eq!(reply.checked_state(&request), Err(Error::WrongOutcome));
    request.command = Command::ResumeSaving;
    reply.outcome = Outcome::SavingResumed;
    assert_eq!(reply.checked_state(&request), Err(Error::UnsafeState));
    reply.state.saving_enabled = true;
    assert!(reply.checked_state(&request).is_ok());
}

/// A stop needs a final-save acknowledgment with admission closed.
#[test]
fn stop_does_not_use_a_generic_health_reply_as_a_save_confirmation() {
    let (mut request, mut reply) = startup();
    request.command = Command::Shutdown;
    assert_eq!(reply.checked_state(&request), Err(Error::WrongOutcome));
    reply.outcome = Outcome::FinalSaveConfirmed;
    reply.state.admission_open = true;
    assert_eq!(reply.checked_state(&request), Err(Error::UnsafeState));
    reply.state.admission_open = false;
    assert!(reply.checked_state(&request).is_ok());
}
