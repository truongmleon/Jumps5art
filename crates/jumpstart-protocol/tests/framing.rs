//! Integration tests for bounded JSON encoding and length-prefixed frame I/O.
//! Exercises malformed input, partial reads, custom serializers, and transport errors.
//!
//! Changes to parsing, buffering, or serialization must preserve rejection rules
//! and stream boundaries. These failure cases check those contracts without
//! requiring a running network service.

use std::io::{self, Cursor, Read, Write};

use jumpstart_protocol::{
    CodecError, MAX_FRAME_BYTES, MAX_JSON_DEPTH, decode_body, encode_body, read_frame, write_frame,
};
use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::{Value, json};

#[test]
/// Verify that short reads reconstruct complete frames in order and stop at EOF.
///
/// Two frames share one byte stream, proving that one read does not consume the
/// next frame and that incomplete next-header input remains an I/O error.
fn round_trip_short_reads_and_consecutive_frames() {
    /// A fixture reader that returns at most three bytes per call.
    ///
    /// Its tuple field owns the encoded stream and current cursor position, making
    /// short successful reads reproducible without a network connection.
    struct ShortReader(Cursor<Vec<u8>>);
    impl Read for ShortReader {
        /// Read up to three bytes from the fixture's encoded stream.
        ///
        /// `self: &mut ShortReader` holds the advancing cursor; `buffer: &mut [u8]` is
        /// the caller's destination. Returns `io::Result<usize>` with the cursor's byte
        /// count, including zero at EOF, and propagates any cursor read error.
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let length = buffer.len().min(3);
            self.0.read(&mut buffer[..length])
        }
    }
    let first = json!({"text": "music 🎵", "position_ms": "9223372036854775807"});
    let second = json!([1, true, null, 0.125]);
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &first).unwrap();
    let first_length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(first_length, bytes.len() - 4);
    write_frame(&mut bytes, &second).unwrap();
    let mut reader = ShortReader(Cursor::new(bytes));
    assert_eq!(read_frame::<_, Value>(&mut reader).unwrap(), first);
    assert_eq!(read_frame::<_, Value>(&mut reader).unwrap(), second);
    assert!(
        matches!(read_frame::<_, Value>(&mut reader), Err(CodecError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
    );
}

#[test]
/// Verify that invalid advertised sizes fail before any body read.
///
/// The fixture panics if the codec reads past its prefix, distinguishing an early
/// length rejection from an error discovered only after touching the body.
fn oversized_or_empty_prefix_never_reads_a_body() {
    /// A reader fixture containing only a four-byte length prefix.
    ///
    /// `prefix: Cursor<[u8; 4]>` tracks consumed header bytes. Any later read panics,
    /// so the test detects attempts to read a body after rejecting its prefix.
    struct HeaderOnly {
        prefix: Cursor<[u8; 4]>,
    }
    impl Read for HeaderOnly {
        /// Read only the remaining length-prefix bytes from the fixture.
        ///
        /// `self: &mut HeaderOnly` holds its cursor; `buffer: &mut [u8]` is the caller's
        /// destination. Returns `io::Result<usize>` from that cursor. Panics if another
        /// read is attempted after all four header bytes were consumed.
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(self.prefix.position() < 4, "body read after invalid prefix");
            self.prefix.read(buffer)
        }
    }
    for length in [MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
        let mut reader = HeaderOnly {
            prefix: Cursor::new(length.to_be_bytes()),
        };
        assert!(matches!(
            read_frame::<_, Value>(&mut reader),
            Err(CodecError::FrameTooLarge { .. })
        ));
    }
    let mut reader = HeaderOnly {
        prefix: Cursor::new(0u32.to_be_bytes()),
    };
    assert!(matches!(
        read_frame::<_, Value>(&mut reader),
        Err(CodecError::EmptyFrame)
    ));
}

#[test]
/// Verify both sides of the exact body-size boundary.
///
/// A maximum-size JSON string round-trips, while larger encoding emits no prefix
/// or body and larger input is rejected before parsing.
fn exact_maximum_body_round_trips_and_larger_body_emits_nothing() {
    let payload = "a".repeat(MAX_FRAME_BYTES - 2);
    let body = encode_body(&payload).unwrap();
    assert_eq!(body.len(), MAX_FRAME_BYTES);
    assert_eq!(decode_body::<String>(&body).unwrap(), payload);
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &payload).unwrap();
    assert_eq!(
        read_frame::<_, String>(&mut Cursor::new(bytes)).unwrap(),
        payload
    );
    let mut destination = Vec::new();
    assert!(matches!(
        write_frame(&mut destination, &"a".repeat(MAX_FRAME_BYTES)),
        Err(CodecError::FrameTooLarge { .. })
    ));
    assert!(destination.is_empty());
    assert!(matches!(
        decode_body::<Value>(&vec![b' '; MAX_FRAME_BYTES + 1]),
        Err(CodecError::FrameTooLarge { .. })
    ));
}

#[test]
/// Verify that missing or partial transport bytes remain `UnexpectedEof` errors.
///
/// The cases cover an absent header, a short header, and a body shorter than the
/// length declared in its complete header.
fn truncated_header_and_body_remain_io_failures() {
    for bytes in [vec![], vec![0, 0, 0], vec![0, 0, 0, 4, b'{', b'}']] {
        assert!(
            matches!(read_frame::<_, Value>(&mut Cursor::new(bytes)), Err(CodecError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
}

#[test]
/// Verify rejection of invalid UTF-8, malformed numbers/strings, and extra JSON.
///
/// A valid finite numeric object with surrounding whitespace remains accepted,
/// so the malformed cases do not accidentally prohibit ordinary input.
fn utf8_json_numbers_surrogates_and_trailing_values_are_checked() {
    assert!(matches!(
        decode_body::<Value>(b"\"\xff\""),
        Err(CodecError::InvalidUtf8(_))
    ));
    for body in [
        b"NaN".as_slice(),
        b"Infinity",
        b"-Infinity",
        b"1e9999",
        b"{}[]",
        br#""\uD800""#,
        b"{\"a\":1,}",
        b"\xef\xbb\xbf{}",
    ] {
        assert!(
            decode_body::<Value>(body).is_err(),
            "accepted malformed body: {body:?}"
        );
    }
    assert_eq!(
        decode_body::<Value>(b" {\"v\": -1.25e2} \n").unwrap(),
        json!({"v": -125.0})
    );
}

#[test]
/// Verify that repeated decoded keys fail even inside nested or future fields.
///
/// The accepted cases distinguish duplicates within one object from equal keys
/// in separate objects and Unicode spellings that JSON does not normalize.
fn duplicate_keys_are_rejected_after_escape_decoding_at_every_depth() {
    for body in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"a":1,"\u0061":2}"#,
        br#"{"nested":[{"x":0,"\u0078":1}]}"#,
        br#"{"ignored_future":{"key":true,"key":false}}"#,
        r#"{"\ud83c\udfb5":1,"🎵":2}"#.as_bytes(),
    ] {
        let error = decode_body::<Value>(body).unwrap_err();
        assert!(matches!(error, CodecError::InvalidJson(_)));
        assert!(error.to_string().contains("duplicate JSON object key"));
    }
    assert!(decode_body::<Value>(br#"[{"x":1},{"x":2}]"#).is_ok());
    // Unicode normalization is not part of JSON key identity.
    assert!(decode_body::<Value>("{\"é\":1,\"é\":2}".as_bytes()).is_ok());
}

#[test]
/// Verify the nesting limit on both incoming and outgoing JSON containers.
///
/// The boundary array is accepted, one deeper is rejected, and bracket characters
/// inside a string do not count as additional nesting.
fn container_depth_is_exact_and_string_brackets_do_not_count() {
    /// Build a valid JSON number wrapped in a requested number of arrays.
    ///
    /// `depth: usize` is the number of enclosing array pairs. Returns `Vec<u8>`
    /// containing ASCII JSON with exactly that container depth and `0` as its leaf.
    fn nested(depth: usize) -> Vec<u8> {
        format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes()
    }
    let accepted: Value = decode_body(&nested(MAX_JSON_DEPTH)).unwrap();
    assert!(encode_body(&accepted).is_ok());
    assert!(decode_body::<Value>(&nested(MAX_JSON_DEPTH + 1)).is_err());
    let too_deep = (0..MAX_JSON_DEPTH + 1).fold(Value::Null, |value, _| Value::Array(vec![value]));
    assert!(matches!(
        encode_body(&too_deep),
        Err(CodecError::NestingTooDeep)
    ));
    assert!(decode_body::<Value>(br#"{"text":"[[[[{{{{"}"#).is_ok());
}

#[test]
/// Verify rejection of NaN/infinity through nested serialization before output.
///
/// Both float widths are exercised; valid finite values and an actual absent
/// optional value still round-trip without being confused with rejected numbers.
fn nonfinite_numbers_are_not_silently_encoded_as_null() {
    /// A derived-serialization fixture that hides floats inside nested containers.
    ///
    /// `values: Vec<Option<f64>>` checks that finite-number validation reaches values
    /// inside both a vector and an optional wrapper.
    #[derive(Serialize)]
    struct Nested {
        values: Vec<Option<f64>>,
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let payload = Nested {
            values: vec![Some(1.0), Some(value)],
        };
        let mut destination = Vec::new();
        assert!(matches!(
            write_frame(&mut destination, &payload),
            Err(CodecError::NonFiniteNumber)
        ));
        assert!(destination.is_empty());
    }
    assert!(matches!(
        encode_body(&vec![f32::NAN]),
        Err(CodecError::NonFiniteNumber)
    ));
    assert_eq!(
        decode_body::<Vec<Option<f64>>>(&encode_body(&vec![Some(1.25), None]).unwrap()).unwrap(),
        vec![Some(1.25), None]
    );
}

#[test]
/// Verify that duplicate keys from a custom serializer fail before any frame bytes.
///
/// Checking only a derived struct's fields would miss this manually emitted map.
fn custom_duplicate_serializers_are_rejected_before_writing() {
    /// A fieldless fixture whose custom serializer deliberately repeats an object key.
    ///
    /// It exercises validation of custom `Serialize` output rather than only derived
    /// structs, whose normal field names do not repeat.
    struct Duplicates;
    impl Serialize for Duplicates {
        /// Offer a map containing the same key twice to the supplied serializer.
        ///
        /// `self: &Duplicates` is the fixture; `serializer: S` implements
        /// `serde::Serializer`. Returns `Result<S::Ok, S::Error>` from the completed map,
        /// propagating any serializer failure while deliberately preserving duplicate entries.
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("a", &1)?;
            map.serialize_entry("a", &2)?;
            map.end()
        }
    }
    let mut destination = Vec::new();
    assert!(matches!(
        write_frame(&mut destination, &Duplicates),
        Err(CodecError::InvalidJson(_))
    ));
    assert!(destination.is_empty());
}

#[test]
/// Verify that a sequence without a length hint cannot exceed the checking budget.
///
/// The custom serializer attempts enough elements to exhaust the allowance, and
/// the encoder must report a size failure instead of completing that output.
fn encoding_budget_stops_a_custom_unbounded_sequence() {
    /// A fieldless fixture that serializes more elements than a frame can admit.
    ///
    /// It supplies no sequence-length hint, so the checker must enforce its shared
    /// budget while traversing elements rather than trusting a declared size.
    struct Many;
    impl Serialize for Many {
        /// Offer an oversized sequence with no declared length to the supplied serializer.
        ///
        /// `self: &Many` is the fixture; `serializer: S` implements `serde::Serializer`.
        /// Returns `Result<S::Ok, S::Error>` from the sequence, propagating the checker's
        /// budget failure as soon as an element cannot be admitted.
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(None)?;
            for _ in 0..MAX_FRAME_BYTES + 10 {
                sequence.serialize_element(&0)?;
            }
            sequence.end()
        }
    }
    assert!(matches!(
        encode_body(&Many),
        Err(CodecError::FrameTooLarge { .. })
    ));
}

#[test]
/// Verify that valid JSON still reports a destination's `BrokenPipe` failure.
///
/// This separates transport I/O errors from payload-validation errors.
fn transport_write_errors_are_preserved() {
    /// A fieldless writer fixture that fails every write with `BrokenPipe`.
    ///
    /// It isolates transport failure handling from JSON validation and serialization.
    struct Broken;
    impl Write for Broken {
        /// Fail a transport write to test I/O error propagation.
        ///
        /// `self: &mut Broken` is the fixture; `_bytes: &[u8]` is ignored output.
        /// Returns `io::Result<usize>` with `Err(BrokenPipe)` without accepting any bytes.
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        /// Complete the fixture's `Write` interface without another injected failure.
        ///
        /// `self: &mut Broken` is borrowed but unchanged. Returns `io::Result<()>` with
        /// `Ok(())`, letting the test isolate the deliberately failing `write` method.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(
        matches!(write_frame(&mut Broken, &json!({})), Err(CodecError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe)
    );
}
