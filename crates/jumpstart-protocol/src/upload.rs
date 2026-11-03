//! Checks upload pieces before placing their bytes into caller-owned staging.
//!
//! A large upload is sent in 4 MiB chunks, with a smaller final chunk when needed.
//! Each piece has a digest so a damaged body can be rejected without touching its
//! destination. This digest covers the piece; the complete resource still needs
//! its own hash and domain checks before publication.
//! The sender also verifies its selected local file piece before emitting bytes,
//! so a changed source does not quietly send a body for an earlier fingerprint.
//!
//! These blocking primitives work with supplied readers/writers. The caller must
//! authorize the transfer, reserve space, and confirm that the target range is
//! unconfirmed staging. Accepted-range replay/conflict checks and synchronized
//! bitmap checkpoints belong to the durable transfer session, not this module.

use std::io::{self, Read, Seek, SeekFrom, Write};

use jumpstart_domain::{BlobId, Counter};
use serde::Serialize;
use thiserror::Error;

/// Fixed upload piece size; only the final piece may contain fewer bytes.
pub const UPLOAD_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// A chunk's checked position, exact length, and expected content fingerprint.
///
/// For example, a 5 MiB resource has a 4 MiB first chunk and a 1 MiB final chunk
/// starting at offset 4 MiB. Private fields preserve that layout check. Construct
/// this value against actual resource metadata with `new`; it has no unchecked
/// deserializer. Its JSON description contains decimal-string offset/length and
/// a tagged chunk digest. It does not identify or authorize a transfer session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct UploadChunk {
    offset: Counter,
    length: Counter,
    chunk_hash: BlobId,
}

impl UploadChunk {
    /// Check one upload piece without allocating or reading its body.
    ///
    /// `offset: Counter` starts at a multiple of 4 MiB; `length: Counter` must be
    /// exactly the remaining piece size, up to 4 MiB. `resource_bytes: Counter`
    /// is the actual declared resource length and `chunk_hash: BlobId` fingerprints
    /// this piece. Returns `Result<UploadChunk, UploadChunkError>`. Empty resources
    /// have no chunks; zero, oversized, short nonfinal, unaligned, and beyond-EOF
    /// pieces are refused before body I/O. Subtraction avoids overflowing an end.
    pub fn new(
        offset: Counter,
        length: Counter,
        resource_bytes: Counter,
        chunk_hash: BlobId,
    ) -> Result<Self, UploadChunkError> {
        if offset >= resource_bytes {
            return Err(UploadChunkError::OutsideResource {
                offset,
                resource_bytes,
            });
        }
        if !offset.value().is_multiple_of(UPLOAD_CHUNK_BYTES) {
            return Err(UploadChunkError::UnalignedOffset { offset });
        }
        let expected = (resource_bytes.value() - offset.value()).min(UPLOAD_CHUNK_BYTES);
        if length.value() != expected {
            return Err(UploadChunkError::UnexpectedLength {
                expected_bytes: expected,
                found: length,
            });
        }
        Ok(Self {
            offset,
            length,
            chunk_hash,
        })
    }

    /// Return this copied `UploadChunk`'s first byte as a `Counter`.
    /// The accepted offset is aligned to the fixed chunk grid.
    pub fn offset(self) -> Counter {
        self.offset
    }

    /// Return this copied chunk's exact body byte count as a `Counter`.
    /// The result is nonzero and at most 4 MiB.
    pub fn length(self) -> Counter {
        self.length
    }

    /// Return this copied chunk's expected `BlobId` without hashing any data.
    /// A full-resource fingerprint is checked separately before publication.
    pub fn chunk_hash(self) -> BlobId {
        self.chunk_hash
    }
}

/// Distinguishes an invalid upload layout, damaged bytes, and actual I/O failure.
///
/// A layout error requires corrected metadata; a digest mismatch rejects the body.
/// I/O failures can require retry/reconciliation and preserve the underlying cause.
/// No variant represents a durable checkpoint or accepted library content.
#[derive(Debug, Error)]
pub enum UploadChunkError {
    /// No chunk begins at or beyond EOF, including an empty resource.
    #[error("upload chunk offset {offset} is outside resource size {resource_bytes}")]
    OutsideResource {
        offset: Counter,
        resource_bytes: Counter,
    },
    /// Piece offsets must follow the fixed grid to avoid ambiguous overlaps.
    #[error("upload chunk offset {offset} is not aligned to {UPLOAD_CHUNK_BYTES} bytes")]
    UnalignedOffset { offset: Counter },
    /// Every nonfinal piece is full-sized, and the final piece ends exactly at EOF.
    #[error("upload chunk requires {expected_bytes} bytes, received declared length {found}")]
    UnexpectedLength { expected_bytes: u64, found: Counter },
    /// A full body was received but differs from its expected piece fingerprint.
    #[error("upload chunk fingerprint mismatch: expected {expected}, received {actual}")]
    DigestMismatch { expected: BlobId, actual: BlobId },
    /// Reading, seeking, or writing failed; partial staging may remain after writes.
    #[error("upload chunk I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Receive a whole chunk, verify its digest, then copy it to unconfirmed staging.
///
/// `source: &mut R` implements `Read` over the raw body, `destination: &mut W`
/// implements `Write + Seek`, and `chunk: UploadChunk` supplies the checked layout
/// and digest. Returns `Result<Counter, UploadChunkError>` with its byte count
/// after all writes. Reads exactly the body length into at most 4 MiB of memory;
/// later stream bytes remain unread. A short or mismatched body never seeks or
/// writes the destination. Interrupted reads/seeks and short writes are handled.
/// Holding one bounded body lets verification finish before the first write.
///
/// The destination is sought to the accepted offset only after verification.
/// Write failures can leave partial staging, which the caller must reconcile.
/// This function does not flush or synchronize bytes, save a bitmap, deduplicate
/// already accepted chunks, enforce session ownership, or publish content. The
/// caller checks replay/overlap rules before invoking it and acknowledges progress
/// only after bytes and checkpoint metadata are durably synchronized. Deadlines,
/// cancellation, and admission limits also remain with that caller.
pub fn copy_upload_chunk<R: Read, W: Write + Seek>(
    source: &mut R,
    destination: &mut W,
    chunk: UploadChunk,
) -> Result<Counter, UploadChunkError> {
    let bytes = read_verified_body(source, chunk)?;
    seek_chunk_offset(destination, chunk.offset)?;
    destination.write_all(&bytes)?;
    Ok(chunk.length)
}

/// Verify one source-file piece before writing its outgoing raw body.
///
/// `source: &mut R` implements `Read + Seek` over the resource;
/// `destination: &mut W` implements `Write` for the outgoing body. `chunk: UploadChunk`
/// holds the checked position/length and expected piece fingerprint. Returns
/// `Result<Counter, UploadChunkError>` after exactly the body bytes are written.
/// Seeks to the piece's resource offset and buffers at most 4 MiB for verification.
/// A changed, truncated, or unreadable piece emits no body bytes. The source's
/// position may advance on failure. Short writes and interrupted I/O are handled.
///
/// The caller sends control metadata separately and binds the source identity,
/// transfer ownership, and expected fingerprint to its session. This function
/// emits neither a JSON frame nor a durable acknowledgment. A destination error
/// can leave an incomplete body, so callers must abort/reconcile the transfer;
/// successful writing does not establish server acceptance or full-file integrity.
/// Flushing, cancellation, deadlines, and resumable progress belong to the caller.
pub fn write_upload_chunk<R: Read + Seek, W: Write>(
    source: &mut R,
    destination: &mut W,
    chunk: UploadChunk,
) -> Result<Counter, UploadChunkError> {
    seek_chunk_offset(source, chunk.offset)?;
    let bytes = read_verified_body(source, chunk)?;
    destination.write_all(&bytes)?;
    Ok(chunk.length)
}

/// Read one bounded body and compare its fingerprint before any output writes.
///
/// `source: &mut R` is positioned at the body's start; `chunk: UploadChunk`
/// supplies its checked length and digest. Returns
/// `Result<Vec<u8>, UploadChunkError>` containing the verified bytes. Both sender and receiver
/// use this check, while their public functions retain different seek ordering.
fn read_verified_body<R: Read>(
    source: &mut R,
    chunk: UploadChunk,
) -> Result<Vec<u8>, UploadChunkError> {
    let mut bytes = vec![0; chunk.length.value() as usize];
    source.read_exact(&mut bytes)?;
    let actual = BlobId::hash_bytes(&bytes);
    if actual != chunk.chunk_hash {
        return Err(UploadChunkError::DigestMismatch {
            expected: chunk.chunk_hash,
            actual,
        });
    }
    Ok(bytes)
}

/// Seek a supplied stream to `offset: Counter`, retrying interrupted seeks.
///
/// `stream: &mut S` implements `Seek`. Returns `Result<(), UploadChunkError>`;
/// genuine errors retain their I/O cause. Receivers call only after verification,
/// while senders position their resource before reading the selected piece.
fn seek_chunk_offset<S: Seek>(stream: &mut S, offset: Counter) -> Result<(), UploadChunkError> {
    loop {
        match stream.seek(SeekFrom::Start(offset.value())) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            outcome => {
                outcome?;
                return Ok(());
            }
        }
    }
}
