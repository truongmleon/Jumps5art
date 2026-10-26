//! Version 1 wire framing and handshake contracts, independent of any transport.
//!
//! No listener, authentication bypass, application RPC, or bulk transfer service is
//! implemented here. Callers authenticate the transport before using regular frames.

mod codec;
mod finite;
mod handshake;

pub use codec::{
    CodecError, FRAME_PREFIX_BYTES, MAX_FRAME_BYTES, MAX_JSON_DEPTH, decode_body, encode_body,
    read_frame, write_frame,
};
pub use handshake::{
    Capability, ErrorCode, HandshakeValidationError, Hello, NegotiationError, ProtocolError,
    ProtocolLimits, ServerHandshake, Welcome, negotiate,
};

/// The regular application protocol; pairing uses its own restricted listener.
pub const APPLICATION_ALPN: &[u8] = b"jumpstart/1";
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
