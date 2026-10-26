use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use jumpstart_domain::{ClientInstanceId, Counter, ServerId, SyncEpoch};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{MAX_FRAME_BYTES, MAX_JSON_DEPTH, PROTOCOL_MAJOR, PROTOCOL_MINOR};

const MAX_CAPABILITIES: usize = 64;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_BUILD_BYTES: usize = 128;
const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

/// A negotiated wire feature, not a product's current health/availability flag.
/// Unknown well-formed names can be offered as optional features by newer peers.
/// The private `String` is nonempty, at most 128 bytes, begins with a lowercase
/// ASCII letter, and contains only lowercase letters, digits, or `._-/`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Capability(String);

impl Capability {
    /// Construct a feature name after checking its wire-format spelling.
    ///
    /// # Arguments
    ///
    /// - `value`: any `Into<String>` value, consumed to obtain the owned feature name.
    ///
    /// # Returns
    ///
    /// `Result<Capability, HandshakeValidationError>` containing the validated name.
    ///
    /// # Errors
    ///
    /// Returns `InvalidCapability` for an empty, oversized, or disallowed name.
    pub fn new(value: impl Into<String>) -> Result<Self, HandshakeValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CAPABILITY_BYTES
            || !value.as_bytes()[0].is_ascii_lowercase()
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
        {
            return Err(HandshakeValidationError::InvalidCapability);
        }
        Ok(Self(value))
    }

    /// Borrow the stored feature name without allocating or transferring ownership.
    ///
    /// # Arguments
    ///
    /// - `self`: a shared `&Capability` reference to the validated feature.
    ///
    /// # Returns
    ///
    /// A `&str` slice that remains valid while `self` is borrowed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Capability {
    /// Write the feature name using Rust's display-formatting interface.
    ///
    /// # Arguments
    ///
    /// - `self`: the shared `&Capability` being displayed.
    /// - `formatter`: the mutable `fmt::Formatter` receiving the name.
    ///
    /// # Returns
    ///
    /// `fmt::Result`: `Ok(())` after writing, or the formatter's write error.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl FromStr for Capability {
    type Err = HandshakeValidationError;
    /// Parse a borrowed name through the same checks as `Capability::new`.
    ///
    /// # Arguments
    ///
    /// - `value`: the `&str` spelling to copy into an owned capability.
    ///
    /// # Returns
    ///
    /// `Result<Capability, HandshakeValidationError>`; invalid spellings return
    /// `InvalidCapability`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}
impl TryFrom<String> for Capability {
    type Error = HandshakeValidationError;
    /// Validate an owned name, including names received through Serde.
    ///
    /// # Arguments
    ///
    /// - `value`: the `String` whose ownership transfers to the constructor.
    ///
    /// # Returns
    ///
    /// `Result<Capability, HandshakeValidationError>`; invalid spellings return
    /// `InvalidCapability`.
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<Capability> for String {
    /// Consume a validated capability and recover its owned spelling.
    ///
    /// # Arguments
    ///
    /// - `value`: the `Capability` to move out of its wrapper.
    ///
    /// # Returns
    ///
    /// The inner `String`, without a further validation step or text copy.
    fn from(value: Capability) -> Self {
        value.0
    }
}

/// Describes malformed handshake fields and invalid relationships between them.
///
/// Variants distinguish feature spelling/list errors, build identifiers, limits,
/// feed ranges, and error-envelope fields. Version compatibility failures instead
/// use `NegotiationError`.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HandshakeValidationError {
    #[error(
        "capability must be 1..=128 lowercase ASCII letters/digits or ._-/ and begin with a letter"
    )]
    InvalidCapability,
    #[error("feature lists contain at most 64 distinct entries")]
    InvalidFeatureList,
    #[error("required features must also appear in supported_features")]
    RequiredFeatureNotOffered,
    #[error("build identifiers must be 1..=128 bytes without control characters")]
    InvalidBuild,
    #[error("wire limits are zero or exceed this implementation's supported bounds")]
    InvalidLimits,
    #[error("minimum change sequence cannot exceed latest sequence plus one")]
    InvalidChangeRange,
    #[error("error state must be 1..=128 bytes without control characters")]
    InvalidErrorState,
    #[error("error message must be 1..=4096 bytes")]
    InvalidErrorMessage,
    #[error("a non-retryable error cannot request a retry delay")]
    NonRetryableDelay,
}

/// Client handshake offer containing protocol versions, build, instance, and features.
///
/// `required_features` must be a subset of the distinct `supported_features` list.
/// Construction and deserialization validate field bounds; callers that directly
/// edit public fields must validate again before using the offer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "HelloWire")]
pub struct Hello {
    pub major: u16,
    pub minor: u16,
    pub client_build: String,
    pub client_instance_id: ClientInstanceId,
    pub supported_features: Vec<Capability>,
    pub required_features: Vec<Capability>,
}

/// Private deserialization shape for the client's handshake fields.
///
/// Keeps incoming versions, build, typed instance ID, and feature lists until
/// `TryFrom<HelloWire>` checks their cross-field relationships.
#[derive(Deserialize)]
struct HelloWire {
    major: u16,
    minor: u16,
    client_build: String,
    client_instance_id: ClientInstanceId,
    supported_features: Vec<Capability>,
    required_features: Vec<Capability>,
}

impl Hello {
    /// Build a validated client offer using this crate's protocol major and minor.
    ///
    /// # Arguments
    ///
    /// - `client_build`: an `Into<String>` build identifier, owned by the offer.
    /// - `client_instance_id`: the typed `ClientInstanceId` of the local app database.
    /// - `supported_features`: a `Vec<Capability>` of distinct supported wire features.
    /// - `required_features`: a `Vec<Capability>` subset needed for this connection.
    ///
    /// # Returns
    ///
    /// `Result<Hello, HandshakeValidationError>` containing the complete offer.
    ///
    /// # Errors
    ///
    /// Returns a validation error for invalid build text, duplicate/oversized lists,
    /// or required features that were not also offered as supported.
    pub fn new(
        client_build: impl Into<String>,
        client_instance_id: ClientInstanceId,
        supported_features: Vec<Capability>,
        required_features: Vec<Capability>,
    ) -> Result<Self, HandshakeValidationError> {
        let hello = Self {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            client_build: client_build.into(),
            client_instance_id,
            supported_features,
            required_features,
        };
        hello.validate()?;
        Ok(hello)
    }

    /// Check build bounds, feature-list bounds, and required-feature membership.
    ///
    /// # Arguments
    ///
    /// - `self`: a shared `&Hello` reference; validation does not modify the offer.
    ///
    /// # Returns
    ///
    /// `Result<(), HandshakeValidationError>`: `Ok(())` for valid fields, or the
    /// first build/list/subset violation. Version negotiation happens separately.
    pub fn validate(&self) -> Result<(), HandshakeValidationError> {
        validate_build(&self.client_build)?;
        validate_features(&self.supported_features)?;
        validate_features(&self.required_features)?;
        if self
            .required_features
            .iter()
            .any(|feature| !self.supported_features.contains(feature))
        {
            return Err(HandshakeValidationError::RequiredFeatureNotOffered);
        }
        Ok(())
    }
}

impl TryFrom<HelloWire> for Hello {
    type Error = HandshakeValidationError;
    /// Convert deserialized client fields into a validated offer.
    ///
    /// # Arguments
    ///
    /// - `wire`: the owned `HelloWire` whose fields move into the offer.
    ///
    /// # Returns
    ///
    /// `Result<Hello, HandshakeValidationError>`; field or subset violations are
    /// returned before the offer is accepted.
    fn try_from(wire: HelloWire) -> Result<Self, Self::Error> {
        let hello = Self {
            major: wire.major,
            minor: wire.minor,
            client_build: wire.client_build,
            client_instance_id: wire.client_instance_id,
            supported_features: wire.supported_features,
            required_features: wire.required_features,
        };
        hello.validate()?;
        Ok(hello)
    }
}

/// Advertised defaults are enforcement budgets, not implemented RPC services.
/// Fields use the shared decimal-string counter encoding, including byte sizes.
/// Counters describe frame/depth, request/stream, subscription, page, deadline,
/// and range budgets. Validate public-field overrides before advertising them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "LimitsWire")]
pub struct ProtocolLimits {
    pub max_frame_bytes: Counter,
    pub max_json_depth: Counter,
    pub max_control_requests: Counter,
    pub max_bulk_streams: Counter,
    pub max_subscription_events: Counter,
    pub max_subscription_bytes: Counter,
    pub max_page_records: Counter,
    pub max_page_bytes: Counter,
    pub control_deadline_ms: Counter,
    pub max_range_bytes: Counter,
}

/// Private incoming limit fields, decoded as bounded decimal-string counters.
///
/// The conversion into `ProtocolLimits` additionally checks nonzero budgets and
/// relationships between page, frame, and implementation limits.
#[derive(Deserialize)]
struct LimitsWire {
    max_frame_bytes: Counter,
    max_json_depth: Counter,
    max_control_requests: Counter,
    max_bulk_streams: Counter,
    max_subscription_events: Counter,
    max_subscription_bytes: Counter,
    max_page_records: Counter,
    max_page_bytes: Counter,
    control_deadline_ms: Counter,
    max_range_bytes: Counter,
}

impl Default for ProtocolLimits {
    /// Return the version-one `ProtocolLimits` budgets used in this crate.
    ///
    /// # Returns
    ///
    /// A `ProtocolLimits` value with the codec ceilings and default request, transfer,
    /// subscription, page, deadline, and range budgets; no services are started.
    fn default() -> Self {
        /// Wrap an internal numeric limit as a domain counter.
        ///
        /// # Arguments
        ///
        /// - `value`: the `u64` compile-time budget supplied by `default`.
        ///
        /// # Returns
        ///
        /// A `Counter` with that numeric value and decimal-string serialization.
        ///
        /// # Panics
        ///
        /// Panics if a supplied constant no longer fits the domain counter range.
        fn counter(value: u64) -> Counter {
            Counter::from_u64(value).expect("constant protocol limit fits a domain counter")
        }
        Self {
            max_frame_bytes: counter(MAX_FRAME_BYTES as u64),
            max_json_depth: counter(MAX_JSON_DEPTH as u64),
            max_control_requests: counter(32),
            max_bulk_streams: counter(4),
            max_subscription_events: counter(256),
            max_subscription_bytes: counter(4 * 1024 * 1024),
            max_page_records: counter(500),
            max_page_bytes: counter(MAX_FRAME_BYTES as u64),
            control_deadline_ms: counter(15_000),
            max_range_bytes: counter(8 * 1024 * 1024),
        }
    }
}

impl ProtocolLimits {
    /// Validate positive budgets and the codec/page bounds enforced here.
    ///
    /// # Arguments
    ///
    /// - `self`: the shared `&ProtocolLimits` being checked.
    ///
    /// # Returns
    ///
    /// `Result<(), HandshakeValidationError>`: `Ok(())` when every budget is
    /// nonzero, frame/depth stay within codec ceilings, and pages fit frames.
    /// Otherwise returns `InvalidLimits`.
    pub fn validate(&self) -> Result<(), HandshakeValidationError> {
        let values = [
            self.max_frame_bytes,
            self.max_json_depth,
            self.max_control_requests,
            self.max_bulk_streams,
            self.max_subscription_events,
            self.max_subscription_bytes,
            self.max_page_records,
            self.max_page_bytes,
            self.control_deadline_ms,
            self.max_range_bytes,
        ];
        if values.iter().any(|value| value.value() == 0)
            || self.max_frame_bytes.value() > MAX_FRAME_BYTES as u64
            || self.max_json_depth.value() > MAX_JSON_DEPTH as u64
            || self.max_page_bytes > self.max_frame_bytes
        {
            return Err(HandshakeValidationError::InvalidLimits);
        }
        Ok(())
    }
}

impl TryFrom<LimitsWire> for ProtocolLimits {
    type Error = HandshakeValidationError;
    /// Move incoming budgets into the public limit type and validate them.
    ///
    /// # Arguments
    ///
    /// - `wire`: the owned `LimitsWire` containing typed incoming counters.
    ///
    /// # Returns
    ///
    /// `Result<ProtocolLimits, HandshakeValidationError>`; `InvalidLimits` rejects
    /// zero budgets, unsupported codec ceilings, or pages larger than frames.
    fn try_from(wire: LimitsWire) -> Result<Self, Self::Error> {
        let limits = Self {
            max_frame_bytes: wire.max_frame_bytes,
            max_json_depth: wire.max_json_depth,
            max_control_requests: wire.max_control_requests,
            max_bulk_streams: wire.max_bulk_streams,
            max_subscription_events: wire.max_subscription_events,
            max_subscription_bytes: wire.max_subscription_bytes,
            max_page_records: wire.max_page_records,
            max_page_bytes: wire.max_page_bytes,
            control_deadline_ms: wire.control_deadline_ms,
            max_range_bytes: wire.max_range_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }
}

/// Server handshake selection with authority IDs, wire features, limits, and feed range.
///
/// Deserialization checks field validity, but acceptance also requires
/// `validate_for` against the sent `Hello`. The connection owner separately checks
/// `server_id` and `sync_epoch` against its trusted authority state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "WelcomeWire")]
pub struct Welcome {
    pub major: u16,
    pub minor: u16,
    pub server_id: ServerId,
    pub sync_epoch: SyncEpoch,
    pub server_build: String,
    /// Sorted intersection of peer-supported wire features. Product availability
    /// is a future authenticated Health/capability service, not this feature list.
    pub capabilities: Vec<Capability>,
    pub limits: ProtocolLimits,
    pub latest_seq: Counter,
    pub minimum_seq: Counter,
}

/// Private incoming server reply before complete welcome validation.
///
/// Carries versions, typed authority/epoch IDs, build text, selected capabilities,
/// limits, and the retained change-feed bounds for conversion into `Welcome`.
#[derive(Deserialize)]
struct WelcomeWire {
    major: u16,
    minor: u16,
    server_id: ServerId,
    sync_epoch: SyncEpoch,
    server_build: String,
    capabilities: Vec<Capability>,
    limits: ProtocolLimits,
    latest_seq: Counter,
    minimum_seq: Counter,
}

impl Welcome {
    /// Check the reply's own fields without comparing it to a client offer.
    ///
    /// # Arguments
    ///
    /// - `self`: the shared `&Welcome` containing server-selected values.
    ///
    /// # Returns
    ///
    /// `Result<(), HandshakeValidationError>`: `Ok(())` for valid build,
    /// capabilities, limits, and a minimum sequence no greater than latest plus
    /// one; otherwise returns the first field or feed-range validation error.
    pub fn validate(&self) -> Result<(), HandshakeValidationError> {
        validate_build(&self.server_build)?;
        validate_features(&self.capabilities)?;
        self.limits.validate()?;
        // Empty retained feeds may expose the next sequence as their minimum.
        if self.minimum_seq.value() > self.latest_seq.value().saturating_add(1) {
            return Err(HandshakeValidationError::InvalidChangeRange);
        }
        Ok(())
    }

    /// A decoded welcome is not trusted until it is checked against the exact
    /// hello and the authority/epoch pinned by the connection owner.
    ///
    /// # Arguments
    ///
    /// - `self`: the shared `&Welcome` to check for this connection.
    /// - `hello`: the `&Hello` actually sent by the client.
    ///
    /// # Returns
    ///
    /// `Result<(), NegotiationError>`: `Ok(())` when both messages are valid,
    /// versions are supported, selected features were offered, and all required
    /// features are present. This method does not check pinned authority IDs.
    ///
    /// # Errors
    ///
    /// Returns malformed-field, major-version, invalid-selection, or missing
    /// required-feature errors. The connection owner must check trust separately.
    pub fn validate_for(&self, hello: &Hello) -> Result<(), NegotiationError> {
        hello.validate()?;
        self.validate()?;
        if self.major != PROTOCOL_MAJOR || self.major != hello.major {
            return Err(NegotiationError::MajorVersionMismatch {
                client_major: hello.major,
                server_major: self.major,
            });
        }
        if self.minor > PROTOCOL_MINOR || self.minor > hello.minor {
            return Err(NegotiationError::InvalidServerSelection);
        }
        if self
            .capabilities
            .iter()
            .any(|feature| !hello.supported_features.contains(feature))
        {
            return Err(NegotiationError::InvalidServerSelection);
        }
        let missing = hello
            .required_features
            .iter()
            .filter(|feature| !self.capabilities.contains(feature))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(NegotiationError::UnsupportedRequiredFeatures { features: missing });
        }
        Ok(())
    }
}

impl TryFrom<WelcomeWire> for Welcome {
    type Error = HandshakeValidationError;
    /// Convert incoming server fields into a field-validated reply.
    ///
    /// # Arguments
    ///
    /// - `wire`: the owned `WelcomeWire` whose fields move into the reply.
    ///
    /// # Returns
    ///
    /// `Result<Welcome, HandshakeValidationError>`; invalid fields or feed ranges
    /// return an error. Client-offer and authority checks remain separate steps.
    fn try_from(wire: WelcomeWire) -> Result<Self, Self::Error> {
        let welcome = Self {
            major: wire.major,
            minor: wire.minor,
            server_id: wire.server_id,
            sync_epoch: wire.sync_epoch,
            server_build: wire.server_build,
            capabilities: wire.capabilities,
            limits: wire.limits,
            latest_seq: wire.latest_seq,
            minimum_seq: wire.minimum_seq,
        };
        welcome.validate()?;
        Ok(welcome)
    }
}

/// Local authority-owned inputs; never copied from an unauthenticated request.
///
/// Contains typed server/epoch IDs, build text, supported wire features, limit
/// budgets, and the retained change-feed range used to construct a `Welcome`.
/// Negotiation validates these public fields before selecting a reply.
#[derive(Clone, Debug)]
pub struct ServerHandshake {
    pub server_id: ServerId,
    pub sync_epoch: SyncEpoch,
    pub server_build: String,
    pub supported_features: Vec<Capability>,
    pub limits: ProtocolLimits,
    pub latest_seq: Counter,
    pub minimum_seq: Counter,
}

/// Explains why a client offer and server selection cannot form a valid handshake.
///
/// Wraps malformed-field errors or reports incompatible major versions, required
/// features the server lacks, and selections the client never offered. Variant
/// fields retain the versions or feature names needed for a structured response.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum NegotiationError {
    #[error(transparent)]
    InvalidHandshake(#[from] HandshakeValidationError),
    #[error("protocol major mismatch (client {client_major}, server {server_major})")]
    MajorVersionMismatch {
        client_major: u16,
        server_major: u16,
    },
    #[error("required wire features are unsupported")]
    UnsupportedRequiredFeatures { features: Vec<Capability> },
    #[error("server selected a version or feature not offered by this client")]
    InvalidServerSelection,
}

/// Negotiate v1.0 without starting a listener or enabling any application method.
///
/// # Arguments
///
/// - `hello`: the `&Hello` offer received from the client.
/// - `server`: the `&ServerHandshake` supplied by the local authority.
///
/// # Returns
///
/// `Result<Welcome, NegotiationError>` containing the server identity, budgets,
/// feed range, and sorted intersection of supported capabilities on success.
///
/// # Errors
///
/// Returns invalid-field, major-version, or unsupported-required-feature errors;
/// the constructed reply is also checked against the exact client offer.
pub fn negotiate(hello: &Hello, server: &ServerHandshake) -> Result<Welcome, NegotiationError> {
    hello.validate()?;
    validate_build(&server.server_build)?;
    validate_features(&server.supported_features)?;
    server.limits.validate()?;
    if hello.major != PROTOCOL_MAJOR {
        return Err(NegotiationError::MajorVersionMismatch {
            client_major: hello.major,
            server_major: PROTOCOL_MAJOR,
        });
    }
    let supported = server.supported_features.iter().collect::<BTreeSet<_>>();
    let missing = hello
        .required_features
        .iter()
        .filter(|feature| !supported.contains(feature))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(NegotiationError::UnsupportedRequiredFeatures { features: missing });
    }
    let capabilities = hello
        .supported_features
        .iter()
        .filter(|feature| supported.contains(feature))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let welcome = Welcome {
        major: PROTOCOL_MAJOR,
        // Version 1 currently implements only minor zero; newer optional
        // features can be offered without raising the selected wire version.
        minor: PROTOCOL_MINOR,
        server_id: server.server_id,
        sync_epoch: server.sync_epoch,
        server_build: server.server_build.clone(),
        capabilities,
        limits: server.limits.clone(),
        latest_seq: server.latest_seq,
        minimum_seq: server.minimum_seq,
    };
    welcome.validate_for(hello)?;
    Ok(welcome)
}

/// Closed set of machine-readable protocol failure categories.
///
/// Serde uses snake-case strings such as `waiting_for_storage`; unknown variants
/// are rejected rather than treated as a known failure with different semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Revoked,
    Unsupported,
    Invalid,
    NotFound,
    Unavailable,
    WaitingForSource,
    WaitingForStorage,
    NeedsInput,
    Conflict,
    SequenceGap,
    IdempotencyMismatch,
    ResyncRequired,
    Quota,
    Canceled,
    Failed,
}

/// Structured wire failure with a code, state, explanation, and retry guidance.
///
/// `retry_after_ms` is an optional decimal-string counter and is valid only when
/// `retryable` is true. `details` holds additional typed JSON context. Constructors
/// from wire data validate text bounds and the retry relationship.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ErrorWire")]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub state: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<Counter>,
    pub details: BTreeMap<String, Value>,
}

/// Private incoming error fields before text and retry validation.
///
/// An omitted `details` map defaults to empty; the optional retry delay uses the
/// same typed counter encoding as other wire budgets.
#[derive(Deserialize)]
struct ErrorWire {
    code: ErrorCode,
    state: String,
    message: String,
    retryable: bool,
    retry_after_ms: Option<Counter>,
    #[serde(default)]
    details: BTreeMap<String, Value>,
}

impl ProtocolError {
    /// Check error text bounds and consistency of the optional retry delay.
    ///
    /// # Arguments
    ///
    /// - `self`: the shared `&ProtocolError` to validate without changing it.
    ///
    /// # Returns
    ///
    /// `Result<(), HandshakeValidationError>`: `Ok(())` for a bounded nonempty
    /// state/message and compatible retry fields; otherwise returns the first
    /// state, message, or non-retryable-delay error.
    pub fn validate(&self) -> Result<(), HandshakeValidationError> {
        if self.state.is_empty()
            || self.state.len() > 128
            || self.state.chars().any(char::is_control)
        {
            return Err(HandshakeValidationError::InvalidErrorState);
        }
        if self.message.is_empty() || self.message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err(HandshakeValidationError::InvalidErrorMessage);
        }
        if !self.retryable && self.retry_after_ms.is_some() {
            return Err(HandshakeValidationError::NonRetryableDelay);
        }
        Ok(())
    }
}

impl TryFrom<ErrorWire> for ProtocolError {
    type Error = HandshakeValidationError;
    /// Convert incoming error fields into a validated protocol failure.
    ///
    /// # Arguments
    ///
    /// - `wire`: the owned `ErrorWire`, including its optional delay and detail map.
    ///
    /// # Returns
    ///
    /// `Result<ProtocolError, HandshakeValidationError>`; malformed text or a delay
    /// on a non-retryable error is returned as a validation failure.
    fn try_from(wire: ErrorWire) -> Result<Self, Self::Error> {
        let error = Self {
            code: wire.code,
            state: wire.state,
            message: wire.message,
            retryable: wire.retryable,
            retry_after_ms: wire.retry_after_ms,
            details: wire.details,
        };
        error.validate()?;
        Ok(error)
    }
}

impl From<NegotiationError> for ProtocolError {
    /// Translate a negotiation failure into a non-retryable wire response.
    ///
    /// # Arguments
    ///
    /// - `error`: the owned `NegotiationError` whose explanation is preserved.
    ///
    /// # Returns
    ///
    /// A `ProtocolError` in the `rejected` state: malformed handshakes use `Invalid`,
    /// compatibility failures use `Unsupported`, and relevant versions/features are
    /// included in `details`. No retry delay is requested.
    fn from(error: NegotiationError) -> Self {
        let mut details = BTreeMap::new();
        let code = match &error {
            NegotiationError::InvalidHandshake(_) => ErrorCode::Invalid,
            NegotiationError::MajorVersionMismatch {
                client_major,
                server_major,
            } => {
                details.insert("client_major".into(), Value::from(*client_major));
                details.insert("server_major".into(), Value::from(*server_major));
                ErrorCode::Unsupported
            }
            NegotiationError::UnsupportedRequiredFeatures { features } => {
                details.insert(
                    "required_features".into(),
                    Value::Array(
                        features
                            .iter()
                            .map(|feature| Value::from(feature.as_str()))
                            .collect(),
                    ),
                );
                ErrorCode::Unsupported
            }
            NegotiationError::InvalidServerSelection => ErrorCode::Unsupported,
        };
        Self {
            code,
            state: "rejected".into(),
            message: error.to_string(),
            retryable: false,
            retry_after_ms: None,
            details,
        }
    }
}

/// Check the build-identifier text shared by client and server messages.
///
/// # Arguments
///
/// - `value`: the borrowed `&str` build identifier.
///
/// # Returns
///
/// `Result<(), HandshakeValidationError>`: `Ok(())` for 1..=128 bytes without
/// control characters, or `InvalidBuild` for a violation.
fn validate_build(value: &str) -> Result<(), HandshakeValidationError> {
    if value.is_empty() || value.len() > MAX_BUILD_BYTES || value.chars().any(char::is_control) {
        Err(HandshakeValidationError::InvalidBuild)
    } else {
        Ok(())
    }
}

/// Check the size and uniqueness of a feature list without changing its order.
///
/// # Arguments
///
/// - `features`: the borrowed `&[Capability]` list to inspect.
///
/// # Returns
///
/// `Result<(), HandshakeValidationError>`: `Ok(())` for at most 64 distinct
/// entries, including an empty list, or `InvalidFeatureList` otherwise.
fn validate_features(features: &[Capability]) -> Result<(), HandshakeValidationError> {
    if features.len() > MAX_CAPABILITIES
        || features.iter().collect::<BTreeSet<_>>().len() != features.len()
    {
        Err(HandshakeValidationError::InvalidFeatureList)
    } else {
        Ok(())
    }
}
