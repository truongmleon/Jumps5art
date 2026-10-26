//! Building blocks for messages exchanged between Jumpstart apps and the server.
//! A "protocol" is the agreed format and rules for that conversation.
//!
//! The initial messages implemented here are Hello and Welcome. An app would send
//! Hello to describe its protocol version and supported features. The server would
//! reply with Welcome describing the selected features, server ID, and limits.
//!
//! This Rust library defines those data types, checks compatibility, and converts
//! messages to/from JSON bytes with a length marker. The codec handles the bytes;
//! handshake code decides whether both sides understand compatible communication rules.
//!
//! The current implementation provides functions and tests, without a network
//! listener. Pairing, app commands, and transferring raw song/photo/video bytes
//! require additional implementation. Callers must authenticate a connection
//! before using the regular protocol.

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
