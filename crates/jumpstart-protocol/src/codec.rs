//! Reads and writes length-prefixed JSON frames for the version 1 protocol.
//! Enforces size and nesting limits, rejects duplicate keys and non-finite numbers,
//! and converts validated JSON into typed messages.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Read, Write};
use std::str::Utf8Error;

use serde::Serialize;
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use thiserror::Error;

use crate::finite::{self, PreflightError};

pub const FRAME_PREFIX_BYTES: usize = 4;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Number of simultaneously open JSON containers; a root object has depth one.
pub const MAX_JSON_DEPTH: usize = 32;

/// Failures from framing, JSON validation, payload conversion, or transport I/O.
///
/// Variants retain the original I/O/JSON error where available. `FrameTooLarge`
/// contains an observed size or a conservative lower bound from encoding;
/// `InvalidEncoding` retains a custom serializer's diagnostic.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("a JSON frame cannot have an empty body")]
    EmptyFrame,
    #[error("frame body length {length} exceeds limit {MAX_FRAME_BYTES}")]
    FrameTooLarge { length: usize },
    #[error("frame body is not UTF-8: {0}")]
    InvalidUtf8(#[from] Utf8Error),
    #[error("invalid JSON frame: {0}")]
    InvalidJson(serde_json::Error),
    #[error("frame does not match the requested payload type: {0}")]
    InvalidPayload(serde_json::Error),
    #[error("non-finite floating-point numbers cannot be encoded")]
    NonFiniteNumber,
    #[error("JSON serialization failed: {0}")]
    Serialization(serde_json::Error),
    #[error("JSON nesting exceeds 32 containers")]
    NestingTooDeep,
    #[error("invalid outgoing payload: {0}")]
    InvalidEncoding(String),
}

/// Read exactly one length-prefixed frame. An incomplete header/body is an I/O
/// error, including clean EOF before a new header; callers own connection lifecycle.
/// Reject the declared length before allocating or reading any body bytes.
///
/// # Arguments
/// - `reader: &mut R`: a borrowed byte source implementing `Read`; reading advances it.
/// - `T: DeserializeOwned`: the requested output type, which cannot borrow the local buffer.
///
/// # Returns
/// `Result<T, CodecError>` containing one decoded payload; trailing frames remain unread.
///
/// # Errors
/// Returns an error for incomplete I/O, invalid lengths, malformed JSON, or a payload
/// that cannot become `T`. A failed read may already have consumed input bytes.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, CodecError> {
    let mut prefix = [0; FRAME_PREFIX_BYTES];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    validate_length(length)?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    decode_body(&bytes)
}

/// Validate and encode before writing the prefix, so an invalid payload emits no
/// partial frame. The caller is responsible for flushing its transport buffer.
///
/// # Arguments
/// - `writer: &mut W`: the borrowed destination implementing `Write`.
/// - `payload: &T`: a possibly unsized value implementing `Serialize`.
///
/// # Returns
/// `Result<(), CodecError>`; `Ok(())` means the prefix and body were written, not flushed.
///
/// # Errors
/// Validation/serialization errors occur before writing. An I/O error can occur after
/// part of the frame reached the destination.
pub fn write_frame<W: Write, T: Serialize + ?Sized>(
    writer: &mut W,
    payload: &T,
) -> Result<(), CodecError> {
    let body = encode_body(payload)?;
    // MAX_FRAME_BYTES fits in u32; the bounded encoder checked this already.
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

/// Decode a body received by a transport that already extracted its prefix.
/// Reject duplicate keys before typed deserialization can ignore or overwrite them.
///
/// # Arguments
/// - `body: &[u8]`: borrowed JSON bytes, excluding the four-byte frame prefix.
/// - `T: DeserializeOwned`: the requested independently owned payload type.
///
/// # Returns
/// `Result<T, CodecError>` containing the validated, typed payload.
///
/// # Errors
/// Rejects empty/oversized input, invalid UTF-8/JSON, duplicate keys, excessive nesting,
/// trailing values, and failure to deserialize the validated tree into `T`.
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, CodecError> {
    let value = validated_value(body)?;
    serde_json::from_value(value).map_err(CodecError::InvalidPayload)
}

/// Serialize through a bounded sink. A finite-number preflight prevents serde_json
/// from silently serializing NaN/infinity as null. Structural validation also runs
/// on outgoing JSON, including custom Serialize implementations with duplicate keys.
///
/// # Arguments
/// - `payload: &T`: a borrowed, possibly unsized `Serialize` value to traverse and encode.
///
/// # Returns
/// `Result<Vec<u8>, CodecError>` containing bounded UTF-8 JSON without a frame prefix.
///
/// # Errors
/// Rejects non-finite numbers, excessive depth/size, custom serialization failures,
/// and invalid output such as duplicate keys before returning any body bytes.
pub fn encode_body<T: Serialize + ?Sized>(payload: &T) -> Result<Vec<u8>, CodecError> {
    finite::check(payload).map_err(|error| match error {
        PreflightError::NonFinite => CodecError::NonFiniteNumber,
        PreflightError::DepthExceeded => CodecError::NestingTooDeep,
        PreflightError::TooLarge => CodecError::FrameTooLarge {
            length: MAX_FRAME_BYTES + 1,
        },
        PreflightError::Serialization(message) => CodecError::InvalidEncoding(message),
    })?;
    let mut sink = BoundedBuffer::default();
    if let Err(error) = serde_json::to_writer(&mut sink, payload) {
        return Err(match sink.exceeded_length {
            Some(length) => CodecError::FrameTooLarge { length },
            None => CodecError::Serialization(error),
        });
    }
    validated_value(&sink.bytes)?;
    Ok(sink.bytes)
}

/// Check whether a frame-body size is within the accepted nonempty range.
///
/// # Arguments
/// - `length: usize`: the advertised or measured number of body bytes.
///
/// # Returns
/// `Result<(), CodecError>` with `Ok(())` for `1..=MAX_FRAME_BYTES`.
///
/// # Errors
/// Returns `EmptyFrame` for zero or `FrameTooLarge` above the byte limit.
fn validate_length(length: usize) -> Result<(), CodecError> {
    match length {
        0 => Err(CodecError::EmptyFrame),
        length if length > MAX_FRAME_BYTES => Err(CodecError::FrameTooLarge { length }),
        _ => Ok(()),
    }
}

/// Parse a complete JSON body before any application-specific deserialization.
///
/// # Arguments
/// - `body: &[u8]`: borrowed bytes whose length, encoding, keys, and nesting are checked.
///
/// # Returns
/// `Result<Value, CodecError>` containing an owned JSON tree with unique keys per object.
///
/// # Errors
/// Rejects invalid size/UTF-8/JSON, excessive depth, duplicate keys, or trailing data.
fn validated_value(body: &[u8]) -> Result<Value, CodecError> {
    validate_length(body.len())?;
    std::str::from_utf8(body)?;
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let value = JsonSeed { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(CodecError::InvalidJson)?;
    deserializer.end().map_err(CodecError::InvalidJson)?;
    Ok(value)
}

/// An in-memory `Write` destination that refuses bodies above the frame limit.
///
/// `bytes` owns accepted output; `exceeded_length` records a rejected attempted size
/// so the encoder can distinguish a size failure from another serializer error.
#[derive(Default)]
struct BoundedBuffer {
    bytes: Vec<u8>,
    exceeded_length: Option<usize>,
}

impl Write for BoundedBuffer {
    /// Append one serializer output slice without exceeding the frame-body limit.
    ///
    /// # Arguments
    /// - `self: &mut BoundedBuffer`: the output and any recorded rejected size.
    /// - `bytes: &[u8]`: the next borrowed bytes to append in full.
    ///
    /// # Returns
    /// `io::Result<usize>` containing `bytes.len()` when all bytes were appended.
    ///
    /// # Errors
    /// Returns an I/O error and records the attempted size if appending would exceed
    /// `MAX_FRAME_BYTES`; the previously accepted bytes remain unchanged.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self.bytes.len().saturating_add(bytes.len());
        if length > MAX_FRAME_BYTES {
            self.exceeded_length = Some(length);
            return Err(io::Error::other("frame body exceeds maximum size"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Complete the `Write` interface for an in-memory destination.
    ///
    /// `self: &mut BoundedBuffer` is borrowed but unchanged. Returns `io::Result<()>`
    /// with `Ok(())`; there is no external buffer or disk to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// State passed into each recursive JSON deserialization step.
///
/// `depth: usize` counts already-open enclosing containers, starting at zero for
/// the root. Each child array/object receives a seed at the next depth.
struct JsonSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;

    /// Transfer the current nesting state into Serde's JSON-value visitor.
    ///
    /// # Arguments
    /// - `self: JsonSeed`: consumed state for this value's enclosing depth.
    /// - `deserializer: D`: a `Deserializer<'de>` supplying the next input value.
    ///
    /// # Returns
    /// `Result<Value, D::Error>` containing an owned JSON value.
    ///
    /// # Errors
    /// Propagates syntax/input errors and the visitor's depth or duplicate-key failures.
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(JsonVisitor { depth: self.depth })
    }
}

/// Build an owned JSON tree while enforcing container depth and unique object keys.
///
/// `depth: usize` counts the enclosing containers before the current value opens
/// one. The `'de` trait lifetime describes input; returned strings are owned.
struct JsonVisitor {
    depth: usize,
}

impl JsonVisitor {
    /// Decide whether one additional array or object may open at this depth.
    ///
    /// `self: &JsonVisitor` supplies the enclosing depth; `E: de::Error` is the caller's
    /// parser error type. Returns `Result<(), E>` with `Ok(())` while opening another
    /// container stays within `MAX_JSON_DEPTH`, or a custom `E` when it would exceed it.
    fn check_container<E: de::Error>(&self) -> Result<(), E> {
        if self.depth >= MAX_JSON_DEPTH {
            Err(E::custom("JSON nesting exceeds 32 containers"))
        } else {
            Ok(())
        }
    }
}

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Value;

    /// Describe acceptable input when Serde needs a type-mismatch diagnostic.
    ///
    /// `self: &JsonVisitor` is borrowed; `formatter: &mut fmt::Formatter<'_>` receives
    /// the description. Returns `fmt::Result`, propagating any formatting failure.
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with unique object keys and bounded nesting")
    }

    /// Convert a JSON boolean into the owned JSON tree.
    ///
    /// `self: JsonVisitor` is consumed and `value: bool` is supplied by the parser.
    /// Returns `Result<Value, E>` with the corresponding JSON value; this conversion
    /// itself does not produce the generic parser error `E`.
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    /// Convert a signed JSON integer into the owned JSON tree.
    ///
    /// `self: JsonVisitor` is consumed and `value: i64` is supplied by the parser.
    /// Returns `Result<Value, E>` with the corresponding JSON value; this conversion
    /// itself does not produce the generic parser error `E`.
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    /// Convert an unsigned JSON integer into the owned JSON tree.
    ///
    /// `self: JsonVisitor` is consumed and `value: u64` is supplied by the parser.
    /// Returns `Result<Value, E>` with the corresponding JSON value; this conversion
    /// itself does not produce the generic parser error `E`.
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    /// Convert a finite floating-point number into the owned JSON tree.
    ///
    /// `self: JsonVisitor` is consumed; `value: f64` is the parser's numeric value.
    /// Returns `Result<Value, E>` containing a JSON number, or a custom `E: de::Error`
    /// when the value is NaN or infinite.
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    /// Copy a transient parser string so the resulting tree does not borrow it.
    ///
    /// `self: JsonVisitor` is consumed; `value: &str` is the borrowed text supplied by
    /// Serde. Returns `Result<Value, E>` with an owned JSON string; this conversion
    /// itself does not return the generic parser error `E`.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    /// Move an already owned parser string into the resulting JSON tree.
    ///
    /// `self: JsonVisitor` and `value: String` are consumed. Returns `Result<Value, E>`
    /// with the JSON string without cloning its text; this conversion itself does not
    /// return the generic parser error `E`.
    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    /// Represent Serde's unit value as JSON null.
    ///
    /// Consumes `self: JsonVisitor` and returns `Result<Value, E>` containing
    /// `Value::Null`; no additional argument or parser error is needed for this case.
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    /// Read every array element with a deeper seed and preserve its order.
    ///
    /// # Arguments
    /// - `self: JsonVisitor`: consumed state containing the enclosing container depth.
    /// - `sequence: A`: a `SeqAccess<'de>` cursor advanced until no elements remain.
    ///
    /// # Returns
    /// `Result<Value, A::Error>` containing an owned JSON array.
    ///
    /// # Errors
    /// Propagates element/input errors and rejects excessive depth or invalid nested values.
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        self.check_container()?;
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(JsonSeed {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    /// Read an object while rejecting duplicate keys after JSON escape decoding.
    ///
    /// # Arguments
    /// - `self: JsonVisitor`: consumed state containing the enclosing container depth.
    /// - `map: A`: a `MapAccess<'de>` cursor supplying string keys and child values.
    ///
    /// # Returns
    /// `Result<Value, A::Error>` containing an owned JSON object with unique keys.
    ///
    /// # Errors
    /// Rejects excessive depth and repeated decoded keys, and propagates child/input errors.
    /// Duplicate-key diagnostics omit the potentially private key text.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.check_container()?;
        let mut keys = HashSet::new();
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                // Do not echo potentially private field names into diagnostics.
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let value = map.next_value_seed(JsonSeed {
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
