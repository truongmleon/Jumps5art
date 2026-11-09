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
//! listener. Checked read ranges and a bounded raw-byte copy primitive support
//! future file-transfer handlers separately from JSON control frames. Upload
//! chunks are layout-checked and fingerprinted before staging writes. Pairing,
//! app commands, authenticated transfer sessions, and publication still require
//! implementation. Callers must authenticate a connection before regular protocol use.

mod codec;
mod finite;
mod handshake;
pub mod pairing;
mod range;
pub mod release;
mod upload;

pub use codec::{
    CodecError, FRAME_PREFIX_BYTES, MAX_FRAME_BYTES, MAX_JSON_DEPTH, decode_body, encode_body,
    read_frame, write_frame,
};
pub use handshake::{
    Capability, ErrorCode, HandshakeValidationError, Hello, NegotiationError, ProtocolError,
    ProtocolLimits, ServerHandshake, Welcome, negotiate,
};
pub use range::{MAX_READ_RANGE_BYTES, ReadRange, ReadRangeError, copy_read_range};
pub use upload::{
    UPLOAD_CHUNK_BYTES, UploadChunk, UploadChunkError, copy_upload_chunk, write_upload_chunk,
};

/// The regular application protocol; pairing uses its own restricted listener.
pub const APPLICATION_ALPN: &[u8] = b"jumpstart/1";
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
