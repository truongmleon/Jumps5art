use jumpstart_domain::{
    BlobId, ClientInstanceId, Counter, DeviceId, ItemId, JobId, MAX_COUNTER_VALUE, OperationId,
    Owner, PositiveCounter, ResourceId, ServerId, SyncEpoch, VersionId,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use uuid::Uuid;

/// Checks that JSON encoding followed by decoding preserves a test value.
///
/// # Arguments
/// - `value`: an owned `T` supporting Serde encoding, owned decoding, equality,
///   and debug formatting so failures can display the compared values.
///
/// # Returns
/// Unit (`()`) after the decoded value equals the original.
///
/// # Panics
/// Panics if either Serde operation fails or the decoded value differs.
fn round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(serde_json::from_str::<T>(&encoded).unwrap(), value);
}

/// Confirms that every generated identity uses its declared UUID version and
/// preserves both its text and JSON representation through decoding.
#[test]
fn generated_ids_have_their_declared_version_and_wire_round_trip() {
    macro_rules! check {
        ($id:ident, $version:literal) => {
            let id = $id::new();
            assert_eq!(id.as_uuid().get_version_num(), $version);
            assert_eq!(id.to_string().parse::<$id>().unwrap(), id);
            assert_eq!(serde_json::to_value(id).unwrap(), json!(id.to_string()));
            round_trip(id);
        };
    }
    check!(ServerId, 4);
    check!(SyncEpoch, 4);
    check!(DeviceId, 4);
    check!(ClientInstanceId, 4);
    check!(ItemId, 7);
    check!(VersionId, 7);
    check!(ResourceId, 7);
    check!(JobId, 7);
}

/// Exercises constructor and decoding rejection paths for nil UUIDs, wrong UUID
/// versions or variants, malformed text, and JSON values of the wrong type.
#[test]
fn identity_validation_rejects_nil_wrong_version_and_wrong_variant() {
    let v4 = Uuid::new_v4();
    let v7 = Uuid::now_v7();
    macro_rules! rejects {
        ($id:ident, $wrong_version:expr, $valid:expr) => {
            assert!($id::from_uuid(Uuid::nil()).is_err());
            assert!($id::from_uuid($wrong_version).is_err());
            assert!(serde_json::from_value::<$id>(json!($wrong_version.to_string())).is_err());
            let mut wrong_variant = *$valid.as_bytes();
            wrong_variant[8] &= 0x7f;
            assert!($id::from_uuid(Uuid::from_bytes(wrong_variant)).is_err());
            assert!(serde_json::from_value::<$id>(json!(null)).is_err());
            assert!(serde_json::from_value::<$id>(json!(42)).is_err());
            assert!("not-an-id".parse::<$id>().is_err());
        };
    }
    rejects!(ServerId, v7, v4);
    rejects!(SyncEpoch, v7, v4);
    rejects!(DeviceId, v7, v4);
    rejects!(ClientInstanceId, v7, v4);
    rejects!(ItemId, v4, v7);
    rejects!(VersionId, v4, v7);
    rejects!(ResourceId, v4, v7);
    rejects!(JobId, v4, v7);
}

/// Checks zero, positive, large, and maximum counter values without JSON numeric
/// precision loss, including positive-to-general conversion and named constants.
#[test]
fn counters_preserve_the_entire_signed_64_bit_range_as_strings() {
    for value in [0, 1, (1_u64 << 53) + 1, MAX_COUNTER_VALUE] {
        let counter = Counter::from_u64(value).unwrap();
        assert_eq!(counter.value(), value);
        assert_eq!(
            serde_json::to_value(counter).unwrap(),
            json!(value.to_string())
        );
        round_trip(counter);
        if value != 0 {
            let positive = PositiveCounter::from_u64(value).unwrap();
            assert_eq!(positive.value(), value);
            assert_eq!(Counter::from(positive), counter);
            round_trip(positive);
        }
    }
    assert_eq!(Counter::ZERO.value(), 0);
    assert_eq!(PositiveCounter::ONE.value(), 1);
    assert_eq!(Counter::MAX.value(), MAX_COUNTER_VALUE);
    assert_eq!(PositiveCounter::MAX.value(), MAX_COUNTER_VALUE);
}

/// Verifies that counters reject noncanonical decimal text, numeric JSON, overflow,
/// and zero where a positive counter is required.
#[test]
fn counters_reject_numeric_json_and_noncanonical_or_out_of_range_text() {
    for invalid in [
        "",
        "00",
        "01",
        "+1",
        "-1",
        " 1",
        "1 ",
        "1\n",
        "1.0",
        "1e3",
        "١",
        "９",
        "9223372036854775808",
        "18446744073709551616",
    ] {
        assert!(invalid.parse::<Counter>().is_err(), "accepted {invalid:?}");
        assert!(
            invalid.parse::<PositiveCounter>().is_err(),
            "accepted {invalid:?}"
        );
        assert!(serde_json::from_value::<Counter>(json!(invalid)).is_err());
        assert!(serde_json::from_value::<PositiveCounter>(json!(invalid)).is_err());
    }
    for invalid in [
        json!(0),
        json!(1),
        json!(1.0),
        json!(null),
        json!(true),
        json!([]),
    ] {
        assert!(serde_json::from_value::<Counter>(invalid.clone()).is_err());
        assert!(serde_json::from_value::<PositiveCounter>(invalid).is_err());
    }
    assert!(PositiveCounter::from_u64(0).is_err());
    assert!("0".parse::<PositiveCounter>().is_err());
    assert!(Counter::from_u64(MAX_COUNTER_VALUE + 1).is_err());
    assert!(Counter::from_u64(u64::MAX).is_err());
    assert!(PositiveCounter::from_u64(u64::MAX).is_err());
}

/// Anchors hashing to the known empty-input BLAKE3 vector and checks that digest
/// bytes, tagged text, and JSON preserve the same content identity.
#[test]
fn blob_ids_use_the_tagged_unkeyed_blake3_digest() {
    // The standard BLAKE3 empty-input vector detects an algorithm/encoding change.
    let empty = "b3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    assert_eq!(BlobId::hash_bytes(b"").to_string(), empty);
    let id = BlobId::hash_bytes(b"retained original bytes");
    assert_eq!(BlobId::from_bytes(*id.as_bytes()), id);
    assert_ne!(BlobId::hash_bytes(b"Retained original bytes"), id);
    assert_eq!(id.to_string().parse::<BlobId>().unwrap(), id);
    assert_eq!(serde_json::to_value(id).unwrap(), json!(id.to_string()));
    round_trip(id);
}

/// Confirms that text and Serde parsing reject wrong tags, uppercase or invalid
/// digits, wrong lengths, surrounding whitespace, and non-ASCII digest text.
#[test]
fn blob_id_parser_rejects_unversioned_uppercase_and_malformed_digests() {
    let valid = BlobId::hash_bytes(b"").to_string();
    let invalid = [
        valid.to_uppercase(),
        format!("b3:{}", valid[3..].to_uppercase()),
        valid.replacen("b3:", "sha256:", 1),
        valid[3..].to_owned(),
        valid[..valid.len() - 1].to_owned(),
        format!("{valid}0"),
        valid.replacen('a', "g", 1),
        format!(" {valid}"),
        format!("{valid}\n"),
        format!("b3:{}", "é".repeat(32)),
    ];
    for value in invalid {
        assert!(value.parse::<BlobId>().is_err(), "accepted {value:?}");
        assert!(serde_json::from_value::<BlobId>(json!(value)).is_err());
    }
}

/// Checks that decoding an operation validates its nested identity and sequence
/// types, rejecting a zero sequence or nil originating device.
#[test]
fn operation_identity_validates_nested_ids_and_positive_sequence() {
    let id = OperationId {
        device_id: DeviceId::new(),
        client_instance_id: ClientInstanceId::new(),
        seq: PositiveCounter::from_u64(42).unwrap(),
    };
    round_trip(id);
    let mut encoded = serde_json::to_value(id).unwrap();
    assert_eq!(encoded["seq"], json!("42"));
    encoded["seq"] = json!("0");
    assert!(serde_json::from_value::<OperationId>(encoded).is_err());
    let mut encoded = serde_json::to_value(id).unwrap();
    encoded["device_id"] = json!(Uuid::nil().to_string());
    assert!(serde_json::from_value::<OperationId>(encoded).is_err());
}

/// Confirms the five supported owning-app names round trip exactly and that
/// arbitrary labels, different casing, and null cannot become an owner.
#[test]
fn owning_app_names_are_exact_and_do_not_accept_arbitrary_labels() {
    for (owner, name) in [
        (Owner::Muse, "Muse"),
        (Owner::Photogenic, "Photogenic"),
        (Owner::Mass, "Mass"),
        (Owner::Bato, "Bato"),
        (Owner::Flex, "Flex"),
    ] {
        assert_eq!(serde_json::to_value(owner).unwrap(), json!(name));
        round_trip(owner);
    }
    for invalid in [
        Value::Null,
        json!("mass"),
        json!("Worlds"),
        json!("Administrator"),
    ] {
        assert!(serde_json::from_value::<Owner>(invalid).is_err());
    }
}
