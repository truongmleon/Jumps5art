//! Exercises upload layout checks and verify-before-write raw body handling.
//!
//! Corrupt or incomplete input must not touch staging. A verified chunk may still
//! encounter a write error, so successful byte copying is separate from durable
//! transfer progress. Tests use scratch buffers rather than a network/session.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use jumpstart_domain::{BlobId, Counter};
use jumpstart_protocol::{
    UPLOAD_CHUNK_BYTES, UploadChunk, UploadChunkError, copy_upload_chunk, read_frame, write_frame,
    write_upload_chunk,
};

/// Return a checked `Counter` for `value: u64`; invalid fixture inputs panic.
fn count(value: u64) -> Counter {
    Counter::from_u64(value).unwrap()
}

/// Detect any destination I/O before a body passes its size and digest checks.
struct NoDestinationIo;

impl Seek for NoDestinationIo {
    /// Panic on every attempted seek to `_position: SeekFrom`.
    fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
        panic!("unverified input must not seek the destination")
    }
}

impl Write for NoDestinationIo {
    /// Panic on every attempted write of `_bytes: &[u8]`.
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        panic!("unverified input must not write the destination")
    }

    /// Panic on flush; durability belongs to the caller.
    fn flush(&mut self) -> io::Result<()> {
        panic!("body copying must not flush")
    }
}

/// Fragment a body and interrupt its first read to simulate ordinary stream I/O.
struct FragmentedBody<'a> {
    bytes: Cursor<&'a [u8]>,
    interrupt: bool,
    interrupt_seek: bool,
}

impl Read for FragmentedBody<'_> {
    /// Read at most two bytes into `buffer`, returning an initial interruption.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.interrupt {
            self.interrupt = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        let length = buffer.len().min(2);
        self.bytes.read(&mut buffer[..length])
    }
}

impl Seek for FragmentedBody<'_> {
    /// Seek to `position: SeekFrom`, returning one initial interruption first.
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.interrupt_seek {
            self.interrupt_seek = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.bytes.seek(position)
    }
}

/// Fail a body read with a recognizable underlying transport error.
struct BrokenBody;

impl Read for BrokenBody {
    /// Return the injected error instead of filling `_buffer: &mut [u8]`.
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "body connection reset",
        ))
    }
}

/// Simulate short/interrupted staging I/O and failure after a written prefix.
struct StagingWriter {
    bytes: Cursor<Vec<u8>>,
    interrupt_seek: bool,
    interrupt_write: bool,
    fail_seek: bool,
    fail_after: usize,
    written: usize,
}

impl StagingWriter {
    /// Return a short-writing, initially interrupted destination containing `bytes`.
    /// Its `fail_after: usize` budget controls when writes return BrokenPipe.
    fn new(bytes: Vec<u8>, fail_after: usize) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            interrupt_seek: true,
            interrupt_write: true,
            fail_seek: false,
            fail_after,
            written: 0,
        }
    }
}

impl Seek for StagingWriter {
    /// Seek to `position: SeekFrom`, retry-testing one interruption or failure.
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fail_seek {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "staging seek failed",
            ));
        }
        if self.interrupt_seek {
            self.interrupt_seek = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.bytes.seek(position)
    }
}

impl Write for StagingWriter {
    /// Write at most two bytes of `bytes`, preserving the injected prefix budget.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupt_write {
            self.interrupt_write = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.written == self.fail_after {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "staging write failed",
            ));
        }
        let length = bytes.len().min(2).min(self.fail_after - self.written);
        let written = self.bytes.write(&bytes[..length])?;
        self.written += written;
        Ok(written)
    }

    /// Panic if copying assumes responsibility for flushing the staging file.
    fn flush(&mut self) -> io::Result<()> {
        panic!("caller owns staging synchronization")
    }
}

/// Chunk grids, full nonfinal pieces, exact final lengths, and maximum counters
/// are checked before any body allocation or I/O is possible.
#[test]
fn layout_enforces_the_grid_and_exact_remaining_piece_size() {
    let hash = BlobId::hash_bytes(b"x");
    let first = UploadChunk::new(
        Counter::ZERO,
        count(UPLOAD_CHUNK_BYTES),
        count(UPLOAD_CHUNK_BYTES + 1),
        hash,
    )
    .unwrap();
    assert_eq!(first.offset(), Counter::ZERO);
    assert_eq!(first.length(), count(UPLOAD_CHUNK_BYTES));
    let last = UploadChunk::new(
        count(UPLOAD_CHUNK_BYTES),
        count(1),
        count(UPLOAD_CHUNK_BYTES + 1),
        hash,
    )
    .unwrap();
    assert_eq!(last.chunk_hash(), hash);
    assert_eq!(
        serde_json::to_value(last).unwrap(),
        serde_json::json!({
            "offset": UPLOAD_CHUNK_BYTES.to_string(), "length":"1", "chunk_hash":hash
        })
    );
    assert!(
        UploadChunk::new(
            Counter::ZERO,
            count(UPLOAD_CHUNK_BYTES),
            count(UPLOAD_CHUNK_BYTES),
            hash
        )
        .is_ok()
    );
    for (offset, size) in [
        (Counter::ZERO, Counter::ZERO),
        (count(4), count(4)),
        (Counter::MAX, Counter::MAX),
    ] {
        assert!(matches!(
            UploadChunk::new(offset, Counter::ZERO, size, hash),
            Err(UploadChunkError::OutsideResource { .. })
        ));
    }
    assert!(matches!(
        UploadChunk::new(count(1), count(1), count(2), hash),
        Err(UploadChunkError::UnalignedOffset { .. })
    ));
    for length in [
        0,
        1,
        UPLOAD_CHUNK_BYTES - 1,
        UPLOAD_CHUNK_BYTES + 1,
        Counter::MAX.value(),
    ] {
        assert!(matches!(
            UploadChunk::new(
                Counter::ZERO,
                count(length),
                count(UPLOAD_CHUNK_BYTES + 1),
                hash
            ),
            Err(UploadChunkError::UnexpectedLength { .. })
        ));
    }
    let offset = Counter::MAX.value() / UPLOAD_CHUNK_BYTES * UPLOAD_CHUNK_BYTES;
    let length = Counter::MAX.value() - offset;
    assert!(UploadChunk::new(count(offset), count(length), Counter::MAX, hash).is_ok());
}

/// A verified final piece changes only its chosen staging interval and leaves the
/// following framed control message unread in the incoming stream.
#[test]
fn verified_binary_body_preserves_other_staging_bytes_and_next_frame() {
    let body = [0, 255, b'\n'];
    let chunk = UploadChunk::new(
        count(UPLOAD_CHUNK_BYTES),
        count(3),
        count(UPLOAD_CHUNK_BYTES + 3),
        BlobId::hash_bytes(&body),
    )
    .unwrap();
    let mut stream = body.to_vec();
    let following = serde_json::json!({"next":true});
    write_frame(&mut stream, &following).unwrap();
    let mut source = Cursor::new(stream);
    let mut staging = Cursor::new(vec![9; UPLOAD_CHUNK_BYTES as usize + 4]);
    assert_eq!(
        copy_upload_chunk(&mut source, &mut staging, chunk).unwrap(),
        count(3)
    );
    let bytes = staging.get_ref();
    assert!(
        bytes[..UPLOAD_CHUNK_BYTES as usize]
            .iter()
            .all(|byte| *byte == 9)
    );
    assert_eq!(
        &bytes[UPLOAD_CHUNK_BYTES as usize..UPLOAD_CHUNK_BYTES as usize + 3],
        &body
    );
    assert_eq!(bytes.last(), Some(&9));
    assert_eq!(
        read_frame::<_, serde_json::Value>(&mut source).unwrap(),
        following
    );
}

/// A complete but altered body returns both fingerprints without destination I/O.
#[test]
fn digest_mismatch_does_not_seek_or_write_staging() {
    let expected = BlobId::hash_bytes(b"good");
    let actual = BlobId::hash_bytes(b"evil");
    let chunk = UploadChunk::new(Counter::ZERO, count(4), count(4), expected).unwrap();
    let error =
        copy_upload_chunk(&mut Cursor::new(b"evil"), &mut NoDestinationIo, chunk).unwrap_err();
    assert!(
        matches!(error, UploadChunkError::DigestMismatch { expected: e, actual: a } if e == expected && a == actual)
    );
}

/// Truncation and transport errors never reach staging and retain their causes.
#[test]
fn short_or_failed_bodies_remain_incomplete_without_destination_io() {
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(4),
        count(4),
        BlobId::hash_bytes(b"good"),
    )
    .unwrap();
    let error =
        copy_upload_chunk(&mut Cursor::new(b"goo"), &mut NoDestinationIo, chunk).unwrap_err();
    assert!(matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
    let error = copy_upload_chunk(&mut BrokenBody, &mut NoDestinationIo, chunk).unwrap_err();
    assert!(
        matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::ConnectionReset && e.to_string() == "body connection reset")
    );
}

/// Interrupted and fragmented I/O still stages one complete verified body without
/// flushing or treating a short write as the end of the chunk.
#[test]
fn fragmented_reads_writes_and_interruptions_complete_the_piece() {
    let body = b"payload";
    let chunk =
        UploadChunk::new(Counter::ZERO, count(7), count(7), BlobId::hash_bytes(body)).unwrap();
    let mut source = FragmentedBody {
        bytes: Cursor::new(body.as_slice()),
        interrupt: true,
        interrupt_seek: true,
    };
    let mut staging = StagingWriter::new(vec![9; 8], usize::MAX);
    assert_eq!(
        copy_upload_chunk(&mut source, &mut staging, chunk).unwrap(),
        count(7)
    );
    assert_eq!(staging.bytes.get_ref(), b"payload\x09");
    assert_eq!(staging.written, 7);
}

/// Verified input can still fail during staging; seek failures preserve all data
/// and write failures retain an incomplete prefix without returning success.
#[test]
fn staging_failures_keep_the_original_error_and_partial_output() {
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(4),
        count(4),
        BlobId::hash_bytes(b"good"),
    )
    .unwrap();
    let mut staging = StagingWriter::new(vec![9; 4], 2);
    staging.fail_seek = true;
    let error = copy_upload_chunk(&mut Cursor::new(b"good"), &mut staging, chunk).unwrap_err();
    assert!(
        matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::PermissionDenied)
    );
    assert_eq!(staging.bytes.get_ref(), &[9; 4]);
    staging.fail_seek = false;
    let error = copy_upload_chunk(&mut Cursor::new(b"good"), &mut staging, chunk).unwrap_err();
    assert!(
        matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::BrokenPipe && e.to_string() == "staging write failed")
    );
    assert_eq!(staging.bytes.get_ref(), b"go\x09\x09");
}

/// A full 4 MiB body verifies and copies exactly once, leaving later bytes unread.
#[test]
fn full_sized_chunk_is_accepted_without_consuming_the_following_byte() {
    let mut bytes = vec![0xa5; UPLOAD_CHUNK_BYTES as usize];
    let hash = BlobId::hash_bytes(&bytes);
    bytes.push(42);
    let mut source = Cursor::new(bytes);
    let mut staging = Cursor::new(Vec::new());
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(UPLOAD_CHUNK_BYTES),
        count(UPLOAD_CHUNK_BYTES),
        hash,
    )
    .unwrap();
    assert_eq!(
        copy_upload_chunk(&mut source, &mut staging, chunk).unwrap(),
        count(UPLOAD_CHUNK_BYTES)
    );
    assert_eq!(BlobId::hash_bytes(staging.get_ref()), hash);
    assert_eq!(source.position(), UPLOAD_CHUNK_BYTES);
    let mut next = [0];
    source.read_exact(&mut next).unwrap();
    assert_eq!(next, [42]);
}

/// Send the selected local resource piece, receive it into staging, and preserve
/// the next control frame. Only raw body bytes are emitted by the sender itself.
#[test]
fn sender_and_receiver_preserve_piece_offsets_and_control_boundaries() {
    let body = [0, 255, b'\n'];
    let mut resource = vec![7; UPLOAD_CHUNK_BYTES as usize];
    resource.extend_from_slice(&body);
    let chunk = UploadChunk::new(
        count(UPLOAD_CHUNK_BYTES),
        count(3),
        count(resource.len() as u64),
        BlobId::hash_bytes(&body),
    )
    .unwrap();
    let mut source = Cursor::new(resource);
    let mut outgoing = Vec::new();
    assert_eq!(
        write_upload_chunk(&mut source, &mut outgoing, chunk).unwrap(),
        count(3)
    );
    assert_eq!(outgoing, body);
    assert_eq!(source.position(), UPLOAD_CHUNK_BYTES + 3);
    let next = serde_json::json!({"next":true});
    write_frame(&mut outgoing, &next).unwrap();
    let mut incoming = Cursor::new(outgoing);
    let mut staging = Cursor::new(vec![9; UPLOAD_CHUNK_BYTES as usize + 3]);
    assert_eq!(
        copy_upload_chunk(&mut incoming, &mut staging, chunk).unwrap(),
        count(3)
    );
    assert!(
        staging.get_ref()[..UPLOAD_CHUNK_BYTES as usize]
            .iter()
            .all(|b| *b == 9)
    );
    assert_eq!(&staging.get_ref()[UPLOAD_CHUNK_BYTES as usize..], &body);
    assert_eq!(
        read_frame::<_, serde_json::Value>(&mut incoming).unwrap(),
        next
    );
}

/// Source changes and truncation cannot emit a body for an earlier fingerprint.
#[test]
fn sender_rejects_changed_or_truncated_source_before_output() {
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(4),
        count(4),
        BlobId::hash_bytes(b"good"),
    )
    .unwrap();
    let error =
        write_upload_chunk(&mut Cursor::new(b"evil"), &mut NoDestinationIo, chunk).unwrap_err();
    assert!(matches!(error, UploadChunkError::DigestMismatch { .. }));
    let error =
        write_upload_chunk(&mut Cursor::new(b"goo"), &mut NoDestinationIo, chunk).unwrap_err();
    assert!(matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
}

/// The sender handles interrupted seeks/reads/writes and short I/O at both ends.
#[test]
fn sender_handles_fragmented_resource_and_outgoing_writes() {
    let resource = b"payload";
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(7),
        count(7),
        BlobId::hash_bytes(resource),
    )
    .unwrap();
    let mut source = FragmentedBody {
        bytes: Cursor::new(resource.as_slice()),
        interrupt: true,
        interrupt_seek: true,
    };
    let mut outgoing = StagingWriter::new(Vec::new(), usize::MAX);
    assert_eq!(
        write_upload_chunk(&mut source, &mut outgoing, chunk).unwrap(),
        count(7)
    );
    assert_eq!(outgoing.bytes.get_ref(), resource);
    assert_eq!(source.bytes.position(), 7);
}

/// Inject source failures before body output, to distinguish them from an
/// outgoing write failure that leaves an incomplete already-verified prefix.
struct FaultyResource {
    fail_seek: bool,
}

impl Read for FaultyResource {
    /// Return the original read error instead of filling `_buffer: &mut [u8]`.
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "resource read failed",
        ))
    }
}

impl Seek for FaultyResource {
    /// Seek to `_position: SeekFrom` or return a configured PermissionDenied error.
    fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
        if self.fail_seek {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource seek failed",
            ));
        }
        Ok(0)
    }
}

/// Sender errors preserve their source and retain any partially emitted body.
#[test]
fn sender_preserves_source_and_output_errors_without_false_completion() {
    let chunk = UploadChunk::new(
        Counter::ZERO,
        count(4),
        count(4),
        BlobId::hash_bytes(b"good"),
    )
    .unwrap();
    for (fail_seek, kind, message) in [
        (
            true,
            io::ErrorKind::PermissionDenied,
            "resource seek failed",
        ),
        (
            false,
            io::ErrorKind::ConnectionReset,
            "resource read failed",
        ),
    ] {
        let error = write_upload_chunk(
            &mut FaultyResource { fail_seek },
            &mut NoDestinationIo,
            chunk,
        )
        .unwrap_err();
        assert!(
            matches!(error, UploadChunkError::Io(e) if e.kind() == kind && e.to_string() == message)
        );
    }
    let mut outgoing = StagingWriter::new(Vec::new(), 2);
    let error = write_upload_chunk(&mut Cursor::new(b"good"), &mut outgoing, chunk).unwrap_err();
    assert!(matches!(error, UploadChunkError::Io(e) if e.kind() == io::ErrorKind::BrokenPipe));
    assert_eq!(outgoing.bytes.get_ref(), b"go");
}
