//! Checks accepted file pieces and raw body copying separately from JSON frames.
//!
//! The reader must stop at the selected end, reject truncated bodies, and retain
//! actual I/O failures. Synthetic adapters exercise short/interrupted I/O without
//! claiming that a network session, placement pin, or content hash was verified.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use jumpstart_domain::Counter;
use jumpstart_protocol::{
    MAX_READ_RANGE_BYTES, ReadRange, ReadRangeError, copy_read_range, read_frame, write_frame,
};

/// Convert a known test quantity into `Counter`; invalid fixtures panic.
fn count(value: u64) -> Counter {
    Counter::from_u64(value).unwrap()
}

/// Simulate partial and interrupted resource reads while tracking buffer requests.
/// Optional failures represent a resource becoming unavailable during a copy.
struct FragmentedSource {
    bytes: Cursor<Vec<u8>>,
    chunk: usize,
    interrupt_read: bool,
    interrupt_seek: bool,
    read_failure_at: Option<u64>,
    seek_failure: bool,
    maximum_requested: usize,
}

impl FragmentedSource {
    /// Build an adapter over `bytes: Vec<u8>` with `chunk: usize` read granularity.
    /// Returns `Self`, initially interrupting one seek and one read.
    fn new(bytes: Vec<u8>, chunk: usize) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            chunk,
            interrupt_read: true,
            interrupt_seek: true,
            read_failure_at: None,
            seek_failure: false,
            maximum_requested: 0,
        }
    }
}

impl Read for FragmentedSource {
    /// Fill at most one fixture chunk in `buffer`, returning its count or an
    /// injected `io::Error`. Tracks the caller's original requested buffer length.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.maximum_requested = self.maximum_requested.max(buffer.len());
        if self.interrupt_read {
            self.interrupt_read = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self
            .read_failure_at
            .is_some_and(|at| self.bytes.position() >= at)
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "resource read failed",
            ));
        }
        let length = self.chunk.min(buffer.len());
        self.bytes.read(&mut buffer[..length])
    }
}

impl Seek for FragmentedSource {
    /// Seek to `position: SeekFrom`, returning the position or an injected error.
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.seek_failure {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource seek failed",
            ));
        }
        if self.interrupt_seek {
            self.interrupt_seek = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.bytes.seek(position)
    }
}

/// Simulate short writes, one interruption, and a failure after an accepted prefix.
/// Retained bytes expose partial output so errors cannot masquerade as completion.
struct FragmentedWriter {
    bytes: Vec<u8>,
    interrupt: bool,
    fail_after: usize,
    write_zero: bool,
}

impl FragmentedWriter {
    /// Return an initially interrupted writer that accepts at most two bytes per
    /// write, failing after `fail_after: usize` total bytes; `zero` chooses WriteZero.
    fn new(fail_after: usize, zero: bool) -> Self {
        Self {
            bytes: Vec::new(),
            interrupt: true,
            fail_after,
            write_zero: zero,
        }
    }
}

impl Write for FragmentedWriter {
    /// Accept part of `bytes: &[u8]`, returning its count or the configured error.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupt {
            self.interrupt = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.bytes.len() == self.fail_after {
            return if self.write_zero {
                Ok(0)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "destination write failed",
                ))
            };
        }
        let length = bytes.len().min(2).min(self.fail_after - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..length]);
        Ok(length)
    }

    /// Panic if copying attempts to flush; callers own flush/durability policy.
    fn flush(&mut self) -> io::Result<()> {
        panic!("body copying must not flush its caller's destination")
    }
}

/// Fail immediately if an empty copy performs any source or destination I/O.
struct NoIo;

impl Read for NoIo {
    /// Panic on every attempted read into `_buffer: &mut [u8]`.
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        panic!("unexpected read")
    }
}

impl Seek for NoIo {
    /// Panic on every attempted seek to `_position: SeekFrom`.
    fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
        panic!("unexpected seek")
    }
}

impl Write for NoIo {
    /// Panic on every attempted write of `_bytes: &[u8]`.
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        panic!("unexpected write")
    }

    /// Panic on every attempted flush.
    fn flush(&mut self) -> io::Result<()> {
        panic!("unexpected flush")
    }
}

/// Bounds, cap, exclusive ends, and decimal wire values share one checked path.
#[test]
fn ranges_are_bounded_without_clamping_or_counter_wraparound() {
    let range = ReadRange::new(count(2), count(3), count(5)).unwrap();
    assert_eq!(range.offset(), count(2));
    assert_eq!(range.length(), count(3));
    assert_eq!(range.end(), count(5));
    assert_eq!(
        serde_json::to_value(range).unwrap(),
        serde_json::json!({"offset":"2","length":"3"})
    );
    for (offset, length, size) in [(6, 0, 5), (4, 2, 5), (i64::MAX as u64, 1, i64::MAX as u64)] {
        assert!(matches!(
            ReadRange::new(count(offset), count(length), count(size)),
            Err(ReadRangeError::OutsideResource { .. })
        ));
    }
    assert!(matches!(
        ReadRange::new(Counter::ZERO, count(MAX_READ_RANGE_BYTES + 1), Counter::MAX),
        Err(ReadRangeError::TooLarge { .. })
    ));
    let end = ReadRange::new(Counter::MAX, Counter::ZERO, Counter::MAX).unwrap();
    assert_eq!(end.end(), Counter::MAX);
}

/// Arbitrary binary bytes remain raw, and following bytes are left unread.
#[test]
fn copying_seeks_and_stops_at_the_exact_accepted_end() {
    let bytes = vec![99, 0, 255, b'{', b'}', 7, 42];
    let mut source = Cursor::new(bytes.clone());
    let range = ReadRange::new(count(1), count(4), count(bytes.len() as u64)).unwrap();
    let mut destination = Vec::new();
    assert_eq!(
        copy_read_range(&mut source, &mut destination, range).unwrap(),
        count(4)
    );
    assert_eq!(destination, vec![0, 255, b'{', b'}']);
    assert_eq!(source.position(), 5);
    let mut trailing = Vec::new();
    source.read_to_end(&mut trailing).unwrap();
    assert_eq!(trailing, vec![7, 42]);
}

/// A JSON descriptor, its raw binary body, and a later control frame retain their
/// separate boundaries without interpreting non-UTF-8 file contents as JSON.
#[test]
fn control_and_raw_body_leave_the_following_frame_intact() {
    let body = [0, 255, b'{', b'"', b'}', 128, b'\n', b'\r'];
    let mut resource = vec![7, 7, 7];
    resource.extend_from_slice(&body);
    resource.extend_from_slice(b"unrequested tail");
    let range = ReadRange::new(count(3), count(8), count(resource.len() as u64)).unwrap();
    let mut stream = Vec::new();
    write_frame(&mut stream, &range).unwrap();
    copy_read_range(&mut Cursor::new(resource), &mut stream, range).unwrap();
    let next = serde_json::json!({"next_control_message": true});
    write_frame(&mut stream, &next).unwrap();

    let mut receiver = Cursor::new(stream);
    let accepted: serde_json::Value = read_frame(&mut receiver).unwrap();
    assert_eq!(accepted, serde_json::json!({"offset":"3","length":"8"}));
    let mut received = [0; 8];
    receiver.read_exact(&mut received).unwrap();
    assert_eq!(received, body);
    let following: serde_json::Value = read_frame(&mut receiver).unwrap();
    assert_eq!(following, next);
    assert_eq!(receiver.position(), receiver.get_ref().len() as u64);
}

/// Fragmented/interrupted reads, writes, and seeks reconstruct one whole body.
#[test]
fn interrupted_and_short_io_preserves_the_exact_body() {
    let mut source = FragmentedSource::new(b"prefix:payload:tail".to_vec(), 3);
    let mut destination = FragmentedWriter::new(usize::MAX, false);
    let range = ReadRange::new(count(7), count(7), count(19)).unwrap();
    assert_eq!(
        copy_read_range(&mut source, &mut destination, range).unwrap(),
        count(7)
    );
    assert_eq!(destination.bytes, b"payload");
    assert_eq!(source.bytes.position(), 14);
}

/// A changed/truncated source keeps its partial bytes but never reports success.
#[test]
fn premature_eof_and_read_errors_remain_incomplete() {
    let range = ReadRange::new(Counter::ZERO, count(4), count(4)).unwrap();
    let mut destination = Vec::new();
    let error = copy_read_range(&mut Cursor::new(b"abc"), &mut destination, range).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(destination, b"abc");
    let mut source = FragmentedSource::new(b"abcd".to_vec(), 2);
    source.read_failure_at = Some(2);
    destination.clear();
    let error = copy_read_range(&mut source, &mut destination, range).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(error.to_string(), "resource read failed");
    assert_eq!(destination, b"ab");
}

/// Seek/write errors retain their original causes; a partial write is not success.
#[test]
fn seek_and_write_failures_are_propagated_with_partial_output_retained() {
    let range = ReadRange::new(Counter::ZERO, count(4), count(4)).unwrap();
    let mut source = FragmentedSource::new(b"abcd".to_vec(), 4);
    source.seek_failure = true;
    let error = copy_read_range(&mut source, &mut NoIo, range).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "resource seek failed");
    for (zero, expected) in [
        (false, io::ErrorKind::BrokenPipe),
        (true, io::ErrorKind::WriteZero),
    ] {
        let mut destination = FragmentedWriter::new(2, zero);
        let error =
            copy_read_range(&mut Cursor::new(b"abcd"), &mut destination, range).unwrap_err();
        assert_eq!(error.kind(), expected);
        assert_eq!(destination.bytes, b"ab");
    }
}

/// Empty resources and empty ranges through EOF do not seek, read, write, or flush.
#[test]
fn zero_length_ranges_need_no_io_even_at_the_largest_position() {
    for (offset, size) in [
        (Counter::ZERO, Counter::ZERO),
        (count(5), count(5)),
        (Counter::MAX, Counter::MAX),
    ] {
        let range = ReadRange::new(offset, Counter::ZERO, size).unwrap();
        assert_eq!(
            copy_read_range(&mut NoIo, &mut NoIo, range).unwrap(),
            Counter::ZERO
        );
    }
}

/// The maximum accepted body streams through bounded buffers, without one large
/// read request or consumption of the byte following the accepted range.
#[test]
fn maximum_range_streams_with_a_small_buffer() {
    let length = MAX_READ_RANGE_BYTES as usize;
    let mut source = FragmentedSource::new(vec![0xa5; length + 1], usize::MAX);
    let range = ReadRange::new(
        Counter::ZERO,
        count(MAX_READ_RANGE_BYTES),
        count((length + 1) as u64),
    )
    .unwrap();
    assert_eq!(
        copy_read_range(&mut source, &mut io::sink(), range).unwrap(),
        count(MAX_READ_RANGE_BYTES)
    );
    assert!(source.maximum_requested <= 32 * 1024);
    assert_eq!(source.bytes.position(), MAX_READ_RANGE_BYTES);
}
