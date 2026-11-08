//! Enforces the one-time invitation and explicit approval steps of device pairing.
//!
//! A matching token only creates a pending request. The operator approves the
//! digest of the exact key and connection they inspected. Transport must prove
//! the key and LAN route before constructing the observation passed here.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use jumpstart_domain::{ClientInstanceId, Owner, ServerId};
use thiserror::Error;
use uuid::Uuid;

const LIFETIME: Duration = Duration::from_secs(5 * 60);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const SOURCE_FAILURE_LIMIT: usize = 5;
const WINDOW_FAILURE_LIMIT: usize = 20;

/// One-time information intended for the pairing QR/manual transfer.
pub struct PairingInvitation {
    pub pairing_id: Uuid,
    pub server_id: ServerId,
    pub server_key: [u8; 32],
    pub lan_endpoint: SocketAddr,
    /// Secret capability. The server's window retains only its digest.
    pub token: [u8; 16],
}

/// The installation details displayed when deciding a pending request.
///
/// `public_key` and `remote` must come from the authenticated, on-link transport.
/// App and device details come from the installation and are observations.
pub struct PairingObservation {
    pub public_key: [u8; 32],
    pub remote: SocketAddr,
    pub client_nonce: [u8; 16],
    pub instance: ClientInstanceId,
    pub app: Owner,
    pub platform: String,
    pub name: String,
}

/// The precise request the operator reviews, including its transcript digest.
///
/// Approval consumes this value. The persistence service must commit the
/// returned installation and its first client instance atomically.
pub struct PendingPairing {
    pub request_id: Uuid,
    pub observation: PairingObservation,
    pub transcript: blake3::Hash,
}

/// Why the invitation or approval could not advance.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum PairingError {
    #[error("the operating system could not generate a pairing token")]
    Entropy,
    #[error("the pairing window is closed or expired")]
    Closed,
    #[error("too many recent failed tokens from this source")]
    RateLimited,
    #[error("invalid pairing invitation")]
    InvalidToken,
    #[error("the invitation has already been consumed")]
    Consumed,
    #[error("the operator's decision does not match the pending request")]
    WrongTranscript,
    #[error("the installation key has been revoked")]
    Revoked,
}

/// Holds one ephemeral invitation and at most one pending request.
///
/// The daemon keeps at most one active window. This value is never restored on
/// restart. The failed-attempt list is bounded by the total window limit.
pub struct PairingWindow {
    opened: Instant,
    pairing_id: Uuid,
    token_digest: Option<blake3::Hash>,
    pending: Option<PendingPairing>,
    failures: Vec<(IpAddr, Instant)>,
}

impl PairingWindow {
    /// Create one invitation and a separate server window that retains its digest.
    ///
    /// `now` is monotonic. The caller supplies the server identity, transport
    /// key, and an already selected LAN endpoint. Returns the window and the
    /// one-time invitation, or an error if OS entropy is unavailable.
    pub fn open(
        now: Instant,
        server_id: ServerId,
        server_key: [u8; 32],
        lan_endpoint: SocketAddr,
    ) -> Result<(Self, PairingInvitation), PairingError> {
        let mut token = [0; 16];
        getrandom::fill(&mut token).map_err(|_| PairingError::Entropy)?;
        let pairing_id = Uuid::new_v4();
        Ok((
            Self {
                opened: now,
                pairing_id,
                token_digest: Some(blake3::hash(&token)),
                pending: None,
                failures: Vec::new(),
            },
            PairingInvitation {
                pairing_id,
                server_id,
                server_key,
                lan_endpoint,
                token,
            },
        ))
    }

    /// Consume a matching token and return the request the operator must review.
    ///
    /// `observation` carries connection-bound identity and installation details.
    /// `is_revoked` checks the authority store by public key. A wrong token
    /// counts toward both limits. An expired, revoked, or consumed invitation
    /// never creates a new pending request.
    pub fn submit(
        &mut self,
        now: Instant,
        token: [u8; 16],
        observation: PairingObservation,
        is_revoked: impl FnOnce(&[u8; 32]) -> bool,
    ) -> Result<&PendingPairing, PairingError> {
        self.check_open(now)?;
        let expected = self.token_digest.ok_or(PairingError::Consumed)?;
        let source = observation.remote.ip().to_canonical();
        let recent = self
            .failures
            .iter()
            .filter(|(ip, at)| *ip == source && now.saturating_duration_since(*at) < RATE_WINDOW)
            .count();
        if recent >= SOURCE_FAILURE_LIMIT {
            return Err(PairingError::RateLimited);
        }
        if blake3::hash(&token) != expected {
            self.failures.push((source, now));
            return Err(PairingError::InvalidToken);
        }
        if is_revoked(&observation.public_key) {
            return Err(PairingError::Revoked);
        }

        let request_id = Uuid::new_v4();
        let transcript = transcript(self.pairing_id, request_id, &observation);
        self.token_digest = None;
        self.pending = Some(PendingPairing {
            request_id,
            observation,
            transcript,
        });
        Ok(self.pending.as_ref().expect("pending request was inserted"))
    }

    /// Consume the exact reviewed request if its key remains eligible.
    ///
    /// Returns the approved installation for atomic persistence by the caller.
    /// Repeating approval, using another digest, or revocation before approval
    /// fails. The transport owns the signed receipt and connection lifecycle.
    pub fn approve(
        &mut self,
        now: Instant,
        expected: blake3::Hash,
        is_revoked: impl FnOnce(&[u8; 32]) -> bool,
    ) -> Result<PendingPairing, PairingError> {
        self.check_open(now)?;
        let pending = self.pending.as_ref().ok_or(PairingError::WrongTranscript)?;
        if pending.transcript != expected {
            return Err(PairingError::WrongTranscript);
        }
        if is_revoked(&pending.observation.public_key) {
            self.pending = None;
            return Err(PairingError::Revoked);
        }
        self.pending.take().ok_or(PairingError::WrongTranscript)
    }

    /// Reject the exact pending request; its invitation stays consumed.
    pub fn reject(&mut self, now: Instant, expected: blake3::Hash) -> Result<(), PairingError> {
        self.check_open(now)?;
        if self.pending.as_ref().map(|pending| pending.transcript) != Some(expected) {
            return Err(PairingError::WrongTranscript);
        }
        self.pending = None;
        Ok(())
    }

    /// Reject expired windows, backwards fixture clocks, and exhausted budgets.
    fn check_open(&self, now: Instant) -> Result<(), PairingError> {
        match now.checked_duration_since(self.opened) {
            Some(age) if age < LIFETIME && self.failures.len() < WINDOW_FAILURE_LIMIT => Ok(()),
            _ => Err(PairingError::Closed),
        }
    }
}

/// Bind approval to this window, request, connection key, and displayed details.
///
/// Returns a digest for local comparison. Variable-length fields are prefixed
/// with their byte lengths, so shifting bytes between fields changes the digest.
fn transcript(window: Uuid, request: Uuid, observation: &PairingObservation) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    hash.update(b"jumpstart pairing approval v1\0");
    hash.update(window.as_bytes());
    hash.update(request.as_bytes());
    hash.update(&observation.public_key);
    hash.update(&observation.client_nonce);
    hash.update(observation.instance.as_uuid().as_bytes());
    let address = observation.remote.to_string();
    let app = format!("{:?}", observation.app);
    for field in [&address, &app, &observation.platform, &observation.name] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize()
}
