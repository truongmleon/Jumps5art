use jumpstart_domain::{Counter, ServerId, SyncEpoch};
use jumpstart_protocol::{
    Capability, CodecError, ErrorCode, HandshakeValidationError, Hello, NegotiationError,
    ProtocolError, ProtocolLimits, ServerHandshake, Welcome, decode_body, encode_body, negotiate,
};
use serde_json::{Value, json};

const HELLO: &[u8] = include_bytes!("../../../protocol/v1/fixtures/hello.json");
const WELCOME: &[u8] = include_bytes!("../../../protocol/v1/fixtures/welcome.json");
const UNSUPPORTED: &[u8] =
    include_bytes!("../../../protocol/v1/fixtures/unsupported-required-feature.json");

fn feature(value: &str) -> Capability {
    value.parse().unwrap()
}
fn counter(value: u64) -> Counter {
    Counter::from_u64(value).unwrap()
}
fn server() -> ServerHandshake {
    ServerHandshake {
        server_id: "a2c3e4f5-1234-4abc-8def-0123456789ab"
            .parse::<ServerId>()
            .unwrap(),
        sync_epoch: "b2c3d4e5-2345-4bcd-9ef0-123456789abc"
            .parse::<SyncEpoch>()
            .unwrap(),
        server_build: "0.1.0-fixture".into(),
        supported_features: vec![feature("control-json.v1"), feature("bulk-bytes.v1")],
        limits: ProtocolLimits::default(),
        latest_seq: counter(135),
        minimum_seq: counter(1),
    }
}

#[test]
fn version_one_fixtures_negotiate_and_preserve_counter_strings() {
    let hello: Hello = decode_body(HELLO).unwrap();
    let expected: Welcome = decode_body(WELCOME).unwrap();
    let actual = negotiate(&hello, &server()).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.capabilities,
        vec![feature("bulk-bytes.v1"), feature("control-json.v1")]
    );
    actual.validate_for(&hello).unwrap();
    let encoded: Value = decode_body(&encode_body(&actual).unwrap()).unwrap();
    assert_eq!(encoded["latest_seq"], "135");
    assert_eq!(encoded["limits"]["control_deadline_ms"], "15000");
    assert_eq!(encoded, serde_json::from_slice::<Value>(WELCOME).unwrap());
}

#[test]
fn optional_unknown_fields_and_features_do_not_break_older_peers() {
    let mut hello_json: Value = serde_json::from_slice(HELLO).unwrap();
    hello_json["future_optional"] = json!({"nested": ["retained by a newer peer"]});
    hello_json["minor"] = json!(27);
    let hello: Hello = decode_body(&serde_json::to_vec(&hello_json).unwrap()).unwrap();
    let welcome = negotiate(&hello, &server()).unwrap();
    assert_eq!(welcome.minor, 0);
    assert!(
        !welcome
            .capabilities
            .contains(&feature("optional-future.v3"))
    );
    let mut welcome_json = serde_json::to_value(&welcome).unwrap();
    welcome_json["future_optional"] = json!(true);
    let decoded: Welcome = decode_body(&serde_json::to_vec(&welcome_json).unwrap()).unwrap();
    decoded.validate_for(&hello).unwrap();
}

#[test]
fn unknown_required_feature_returns_typed_unsupported_without_welcome() {
    let mut hello: Hello = decode_body(HELLO).unwrap();
    hello.supported_features.push(feature("future-required.v2"));
    hello.required_features.push(feature("future-required.v2"));
    let negotiation_error = negotiate(&hello, &server()).unwrap_err();
    assert!(
        matches!(&negotiation_error, NegotiationError::UnsupportedRequiredFeatures { features } if features == &vec![feature("future-required.v2")])
    );
    let error = ProtocolError::from(negotiation_error);
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(!error.retryable);
    assert_eq!(error, decode_body::<ProtocolError>(UNSUPPORTED).unwrap());
    assert_eq!(
        decode_body::<ProtocolError>(&encode_body(&error).unwrap()).unwrap(),
        error
    );
}

#[test]
fn major_mismatch_does_not_fall_back_to_an_accepted_session() {
    let mut hello: Hello = decode_body(HELLO).unwrap();
    hello.major = 2;
    let error = negotiate(&hello, &server()).unwrap_err();
    assert!(matches!(
        error,
        NegotiationError::MajorVersionMismatch {
            client_major: 2,
            server_major: 1
        }
    ));
    assert_eq!(ProtocolError::from(error).code, ErrorCode::Unsupported);
}

#[test]
fn client_checks_the_actual_server_selection_against_its_hello() {
    let hello: Hello = decode_body(HELLO).unwrap();
    let original = negotiate(&hello, &server()).unwrap();
    let mut welcome = original.clone();
    welcome.capabilities.push(feature("never-offered.v1"));
    assert!(matches!(
        welcome.validate_for(&hello),
        Err(NegotiationError::InvalidServerSelection)
    ));
    welcome = original.clone();
    welcome
        .capabilities
        .retain(|value| value != &feature("control-json.v1"));
    assert!(matches!(
        welcome.validate_for(&hello),
        Err(NegotiationError::UnsupportedRequiredFeatures { .. })
    ));
    welcome = original.clone();
    welcome.major = 2;
    assert!(matches!(
        welcome.validate_for(&hello),
        Err(NegotiationError::MajorVersionMismatch { .. })
    ));
    welcome = original;
    welcome.minor = 1;
    assert!(matches!(
        welcome.validate_for(&hello),
        Err(NegotiationError::InvalidServerSelection)
    ));
}

#[test]
fn duplicate_keys_inside_unknown_fields_cannot_evade_validation() {
    let hello = std::str::from_utf8(HELLO).unwrap().trim_end();
    let malformed = format!(
        "{},\"ignored\":{{\"x\":1,\"\\u0078\":2}}}}",
        hello.trim_end_matches('}')
    );
    assert!(matches!(
        decode_body::<Hello>(malformed.as_bytes()),
        Err(CodecError::InvalidJson(_))
    ));
}

#[test]
fn hello_deserialization_validates_bounds_ids_and_feature_relations() {
    let original: Value = serde_json::from_slice(HELLO).unwrap();
    let mut cases = Vec::new();
    let mut value = original.clone();
    value["client_build"] = json!("");
    cases.push(value);
    let mut value = original.clone();
    value["client_build"] = json!("x".repeat(129));
    cases.push(value);
    let mut value = original.clone();
    value["client_build"] = json!("private\nvalue");
    cases.push(value);
    let mut value = original.clone();
    value["client_instance_id"] = json!("not-a-uuid");
    cases.push(value);
    let mut value = original.clone();
    value["client_instance_id"] = json!("123e4567-e89b-72d3-a456-426614174000");
    cases.push(value);
    let mut value = original.clone();
    value["supported_features"] = json!(["control-json.v1", "control-json.v1"]);
    cases.push(value);
    let mut value = original.clone();
    value["required_features"] = json!(["never-offered.v1"]);
    cases.push(value);
    let mut value = original.clone();
    value["required_features"] = json!(["control-json.v1", "control-json.v1"]);
    cases.push(value);
    let mut value = original.clone();
    value["supported_features"] = json!(
        (0..65)
            .map(|index| format!("feature-{index}"))
            .collect::<Vec<_>>()
    );
    cases.push(value);
    let mut value = original;
    value["supported_features"] = json!(["Invalid Capability"]);
    cases.push(value);
    for value in cases {
        assert!(
            decode_body::<Hello>(&serde_json::to_vec(&value).unwrap()).is_err(),
            "accepted: {value}"
        );
    }
}

#[test]
fn capability_names_are_bounded_and_deserialization_uses_the_constructor() {
    for name in [
        "",
        "Uppercase",
        "2starts-with-number",
        "has space",
        "feature:secret",
        "x\n",
    ] {
        assert!(Capability::new(name).is_err());
        assert!(serde_json::from_value::<Capability>(json!(name)).is_err());
    }
    assert!(Capability::new("a".repeat(128)).is_ok());
    assert!(Capability::new("a".repeat(129)).is_err());
    assert_eq!(feature("control-json.v1").as_str(), "control-json.v1");
}

#[test]
fn welcome_rejects_numeric_noncanonical_and_out_of_range_counters() {
    let original: Value = serde_json::from_slice(WELCOME).unwrap();
    for invalid in [
        json!(135),
        json!("0135"),
        json!("+135"),
        json!("-1"),
        json!("9223372036854775808"),
    ] {
        let mut value = original.clone();
        value["latest_seq"] = invalid;
        assert!(decode_body::<Welcome>(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut value = original.clone();
    value["minimum_seq"] = json!("137");
    assert!(decode_body::<Welcome>(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value = original;
    value["latest_seq"] = json!("0");
    value["minimum_seq"] = json!("0");
    assert!(decode_body::<Welcome>(&serde_json::to_vec(&value).unwrap()).is_ok());
}

#[test]
fn limit_overrides_cannot_raise_codec_caps_or_make_zero_budgets() {
    let original: Value = serde_json::from_slice(WELCOME).unwrap();
    for (key, invalid) in [
        ("max_frame_bytes", "1048577"),
        ("max_json_depth", "33"),
        ("max_bulk_streams", "0"),
        ("max_page_bytes", "1048577"),
    ] {
        let mut value = original.clone();
        value["limits"][key] = json!(invalid);
        assert!(decode_body::<Welcome>(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut config = server();
    config.limits.max_control_requests = Counter::ZERO;
    let hello: Hello = decode_body(HELLO).unwrap();
    assert!(matches!(
        negotiate(&hello, &config),
        Err(NegotiationError::InvalidHandshake(
            HandshakeValidationError::InvalidLimits
        ))
    ));
}

#[test]
fn errors_are_typed_bounded_and_use_decimal_retry_delays() {
    let mut value: Value = serde_json::from_slice(UNSUPPORTED).unwrap();
    value["future_optional"] = json!({"preserved": false});
    value["code"] = json!("quota");
    value["retryable"] = json!(true);
    value["retry_after_ms"] = json!("60000");
    let decoded: ProtocolError = decode_body(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(decoded.retry_after_ms, Some(counter(60_000)));
    value["retry_after_ms"] = json!(60_000);
    assert!(decode_body::<ProtocolError>(&serde_json::to_vec(&value).unwrap()).is_err());
    value["retry_after_ms"] = json!("60000");
    value["retryable"] = json!(false);
    assert!(decode_body::<ProtocolError>(&serde_json::to_vec(&value).unwrap()).is_err());
    value["retryable"] = json!(true);
    value["code"] = json!("future_unknown_code");
    assert!(decode_body::<ProtocolError>(&serde_json::to_vec(&value).unwrap()).is_err());
    value["code"] = json!("quota");
    value["message"] = json!("x".repeat(4097));
    assert!(decode_body::<ProtocolError>(&serde_json::to_vec(&value).unwrap()).is_err());
}
