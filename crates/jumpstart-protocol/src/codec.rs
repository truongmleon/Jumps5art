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
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, CodecError> {
    let value = validated_value(body)?;
    serde_json::from_value(value).map_err(CodecError::InvalidPayload)
}

/// Serialize through a bounded sink. A finite-number preflight prevents serde_json
/// from silently serializing NaN/infinity as null. Structural validation also runs
/// on outgoing JSON, including custom Serialize implementations with duplicate keys.
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

fn validate_length(length: usize) -> Result<(), CodecError> {
    match length {
        0 => Err(CodecError::EmptyFrame),
        length if length > MAX_FRAME_BYTES => Err(CodecError::FrameTooLarge { length }),
        _ => Ok(()),
    }
}

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

#[derive(Default)]
struct BoundedBuffer {
    bytes: Vec<u8>,
    exceeded_length: Option<usize>,
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self.bytes.len().saturating_add(bytes.len());
        if length > MAX_FRAME_BYTES {
            self.exceeded_length = Some(length);
            return Err(io::Error::other("frame body exceeds maximum size"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct JsonSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(JsonVisitor { depth: self.depth })
    }
}

struct JsonVisitor {
    depth: usize,
}

impl JsonVisitor {
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

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with unique object keys and bounded nesting")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

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
