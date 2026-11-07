//! Checks and copies bounded pieces of immutable resource bytes.
//!
//! Playback or random access can request a file piece instead of the whole file.
//! A range names an offset and length, such as bytes [100, 200). These numbers
//! belong in a control message; the actual file bytes travel separately as raw
//! bytes. Checking against the resource's length prevents wrapping arithmetic,
//! reads beyond EOF, and a request larger than one permitted range stream.
//!
//! This blocking I/O primitive copies from a supplied seekable source. Callers
//! must authorize the request, bind/pin a verified immutable placement, and send
//! its accepted offset/length and blob identity before the body. This module
//! supplies neither a network listener nor resource verification/publication.

use std::io::{self, Read, Seek, SeekFrom, Write};

use jumpstart_domain::Counter;
use serde::Serialize;
use thiserror::Error;

/// Maximum raw body length for one read range; clients request successive ranges.
pub const MAX_READ_RANGE_BYTES: u64 = 8 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 32 * 1024;

/// A half-open resource range checked before reading any of its bytes.
///
/// For example, offset 100 and length 100 accepts bytes [100, 200) only when the
/// supplied resource has at least 200 bytes. Private fields keep callers from
/// bypassing the size and bounds checks. JSON describes only offset/length as
/// decimal strings. There is no deserializer: a request must be checked against
/// the actual resource size through `new`, rather than trusting its own bounds.
/// This value is a range description; it grants no access to a resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReadRange {
    offset: Counter,
    length: Counter,
    #[serde(skip)]
    end: Counter,
}

impl ReadRange {
    /// Check an offset/length against one known resource without performing I/O.
    ///
    /// `offset: Counter` is the first byte, `length: Counter` is the number of
    /// bytes, and `resource_bytes: Counter` is the caller's verified resource size.
    /// Returns `Result<ReadRange, ReadRangeError>`. Empty ranges are valid through
    /// EOF, including an empty resource. Lengths over 8 MiB and ranges past EOF
    /// are refused; overflow cannot turn a large range into an accepted small one.
    pub fn new(
        offset: Counter,
        length: Counter,
        resource_bytes: Counter,
    ) -> Result<Self, ReadRangeError> {
        if length.value() > MAX_READ_RANGE_BYTES {
            return Err(ReadRangeError::TooLarge { length });
        }
        let end = offset
            .value()
            .checked_add(length.value())
            .filter(|end| *end <= resource_bytes.value())
            .and_then(|end| Counter::from_u64(end).ok())
            .ok_or(ReadRangeError::OutsideResource {
                offset,
                length,
                resource_bytes,
            })?;
        Ok(Self {
            offset,
            length,
            end,
        })
    }

    /// Return the first byte's `Counter` from this copied `ReadRange` value.
    /// The range is `Copy`, so the caller retains its original description.
    pub fn offset(self) -> Counter {
        self.offset
    }

    /// Return the accepted raw byte count as a `Counter`, possibly zero.
    /// `self: ReadRange` is copied; the returned count is at most 8 MiB.
    pub fn length(self) -> Counter {
        self.length
    }

    /// Return the exclusive end position as a `Counter` from this copied range.
    /// Construction already checked the sum and the supplied resource bound.
    pub fn end(self) -> Counter {
        self.end
    }
}

/// Explains why a requested file piece cannot be accepted before body I/O.
///
/// A too-large request needs smaller successive ranges; an out-of-bounds request
/// needs corrected offsets or resource metadata. These errors do not read, pad,
/// truncate, or silently clamp the requested range.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ReadRangeError {
    /// The request exceeds the fixed per-stream body limit.
    #[error("read range length {length} exceeds {MAX_READ_RANGE_BYTES} bytes")]
    TooLarge { length: Counter },
    /// The start or exclusive end lies beyond the supplied resource size.
    #[error("read range offset {offset} and length {length} exceed resource size {resource_bytes}")]
    OutsideResource {
        offset: Counter,
        length: Counter,
        resource_bytes: Counter,
    },
}

/// Seek to and copy exactly one accepted range as raw bytes with bounded memory.
///
/// `source: &mut R` implements `Read + Seek`, `destination: &mut W` implements
/// `Write`, and `range: ReadRange` is checked against the bound immutable resource.
/// Returns `io::Result<Counter>` with the accepted byte count only after all bytes
/// are written. A zero-length range performs no I/O. Nonempty copies use a 32 KiB
/// buffer, retry interrupted reads/seeks, and never read beyond the accepted end.
///
/// A short source returns `UnexpectedEof`; other I/O failures retain their cause.
/// Errors can leave a partial destination and advanced source position. No partial
/// body becomes a successful copy, and no output is removed, flushed, synchronized,
/// hashed, or published here. Callers retain responsibility for cancellation,
/// deadlines, integrity checks, durable checkpoints, and handling incomplete data.
pub fn copy_read_range<R: Read + Seek, W: Write>(
    source: &mut R,
    destination: &mut W,
    range: ReadRange,
) -> io::Result<Counter> {
    let mut remaining = range.length.value();
    if remaining == 0 {
        return Ok(Counter::ZERO);
    }
    loop {
        match source.seek(SeekFrom::Start(range.offset.value())) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            outcome => {
                outcome?;
                break;
            }
        }
    }
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining != 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let read = match source.read(&mut buffer[..requested]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            outcome => outcome?,
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "resource ended before its accepted range was fully copied",
            ));
        }
        destination.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    Ok(range.length)
}
