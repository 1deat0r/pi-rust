//! Full port of `packages/protocol/test/cbor/cbor.test.ts` behavioral
//! coverage. Lone-surrogate / symbol / function / Date / Map encoder cases
//! from upstream cannot occur in Rust values and are covered by the encoder
//! surface in unit tests where representable.

use pi_protocol::*;

fn from_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn json_to_value(v: serde_json::Value) -> Value {
    Value::from(v)
}

#[test]
fn encodes_and_decodes_rfc_8949_vectors() {
    let known: Vec<(serde_json::Value, &str)> = vec![
        (serde_json::json!(null), "f6"),
        (serde_json::json!(false), "f4"),
        (serde_json::json!(true), "f5"),
        (serde_json::json!(0), "00"),
        (serde_json::json!(1), "01"),
        (serde_json::json!(10), "0a"),
        (serde_json::json!(23), "17"),
        (serde_json::json!(24), "1818"),
        (serde_json::json!(25), "1819"),
        (serde_json::json!(100), "1864"),
        (serde_json::json!(1000), "1903e8"),
        (serde_json::json!(1_000_000), "1a000f4240"),
        (serde_json::json!(1_000_000_000_000i64), "1b000000e8d4a51000"),
        (serde_json::json!(9_007_199_254_740_991_i64), "1b001fffffffffffff"),
        (serde_json::json!(-1), "20"),
        (serde_json::json!(-10), "29"),
        (serde_json::json!(-24), "37"),
        (serde_json::json!(-25), "3818"),
        (serde_json::json!(-100), "3863"),
        (serde_json::json!(-1000), "3903e7"),
        (serde_json::json!(-1_000_000), "3a000f423f"),
        (serde_json::json!(-9_007_199_254_740_991_i64), "3b001ffffffffffffe"),
        (serde_json::json!(1.1), "fb3ff199999999999a"),
        (serde_json::json!(""), "60"),
        (serde_json::json!("IETF"), "6449455446"),
        (serde_json::json!("ü"), "62c3bc"),
        (serde_json::json!("水"), "63e6b0b4"),
        (serde_json::json!("𐅑"), "64f0908591"),
        (serde_json::json!([]), "80"),
        (serde_json::json!([1, 2, 3]), "83010203"),
        (serde_json::json!([1, [2, 3], [4, 5]]), "8301820203820405"),
        (
            serde_json::json!({"a": 1, "b": [2, 3]}),
            "a26161016162820203",
        ),
    ];
    for (json, wire) in known {
        let value = json_to_value(json);
        let encoded = encode_cbor(&value, &CborOptions::default()).unwrap();
        assert_eq!(to_hex(&encoded), wire);
        let decoded = decode_cbor(&from_hex(wire), &CborOptions::default()).unwrap();
        assert_eq!(decoded, value);
    }
}

#[test]
fn negative_zero_preserved() {
    // JS number -0 encodes as float64; decode round-trips to -0 float.
    let value = Value::Float(-0.0);
    let encoded = encode_cbor(&value, &CborOptions::default()).unwrap();
    assert_eq!(to_hex(&encoded), "fb8000000000000000");
    let decoded = decode_cbor(&encoded, &CborOptions::default()).unwrap();
    assert!(matches!(decoded, Value::Float(f) if f == 0.0 && f.is_sign_negative()));
}

#[test]
fn omits_undefined_properties_without_omitting_falsey_values() {
    let value = Value::Map(vec![
        ("omitted".into(), Value::Undefined),
        ("zero".into(), Value::Int(0)),
        ("empty".into(), Value::Text(String::new())),
        ("no".into(), Value::Bool(false)),
        ("nil".into(), Value::Null),
    ]);
    let encoded = encode_cbor(&value, &CborOptions::default()).unwrap();
    let decoded = decode_cbor(&encoded, &CborOptions::default()).unwrap();
    assert_eq!(decoded, Value::Map(vec![
        ("zero".into(), Value::Int(0)),
        ("empty".into(), Value::Text(String::new())),
        ("no".into(), Value::Bool(false)),
        ("nil".into(), Value::Null),
    ]));
}

#[test]
fn preserves_leading_unicode_bom() {
    let decoded = decode_cbor(&from_hex("63efbbbf"), &CborOptions::default()).unwrap();
    assert_eq!(decoded, Value::Text("\u{feff}".into()));
}

#[test]
fn rejects_unsupported_encoder_values() {
    // Top-level undefined
    assert!(encode_cbor(&Value::Undefined, &CborOptions::default()).is_err());
    // Undefined array element
    assert!(encode_cbor(&Value::Array(vec![Value::Undefined]), &CborOptions::default()).is_err());
    // NaN / infinities
    assert!(encode_cbor(&Value::Float(f64::NAN), &CborOptions::default()).is_err());
    assert!(encode_cbor(&Value::Float(f64::INFINITY), &CborOptions::default()).is_err());
    assert!(encode_cbor(&Value::Float(f64::NEG_INFINITY), &CborOptions::default()).is_err());
    // Unsafe integers
    assert!(encode_cbor(&Value::Int(9_007_199_254_740_991 + 1), &CborOptions::default()).is_err());
    assert!(encode_cbor(&Value::Int(-9_007_199_254_740_991 - 1), &CborOptions::default()).is_err());
}

#[test]
fn rejects_invalid_decoder_inputs() {
    let invalid: Vec<&str> = vec![
        "",                          // empty input
        "18",                        // truncated integer
        "1c",                        // reserved additional information
        "5f",                        // indefinite byte string
        "7f",                        // indefinite text string
        "9f",                        // indefinite array
        "bf",                        // indefinite map
        "c000",                      // tag
        "f7",                        // undefined simple value
        "e0",                        // unsupported simple value
        "ff",                        // break outside an indefinite item
        "f93c00",                    // float16
        "fa3f800000",                // float32
        "fb7ff0000000000000",        // positive infinity
        "fb7ff8000000000000",        // NaN
        "fb3ff00000",                // truncated float64
        "44010203",                  // truncated byte string
        "636162",                    // truncated text string
        "8201",                      // truncated array
        "a16161",                    // truncated map
        "0000",                      // trailing data
        "a10102",                    // non-string map key
        "a2616101616102",            // duplicate map key
        "61ff",                      // invalid UTF-8 byte
        "62c080",                    // overlong UTF-8
        "63eda080",                  // UTF-8 surrogate
        "1b0020000000000000",        // unsafe positive integer
        "3b001fffffffffffff",        // unsafe negative integer
        "fb4340000000000000",        // unsafe integer encoded as float64
    ];
    for hex_str in invalid {
        assert!(
            decode_cbor(&from_hex(hex_str), &CborOptions::default()).is_err(),
            "expected error for {hex_str}"
        );
    }
}

#[test]
fn rejects_excessive_depth_before_traversing() {
    // Build a 66-deep array-of-1 chain ending in null.
    let mut bytes = vec![0x81u8; DEFAULT_MAX_CBOR_DEPTH + 2];
    bytes[DEFAULT_MAX_CBOR_DEPTH + 1] = 0xf6;
    let err = decode_cbor(&bytes, &CborOptions::default()).unwrap_err();
    assert!(err.0.contains("depth"));

    // Encode side: build a Value nested 66 deep.
    let mut value = Value::Null;
    for _ in 0..=DEFAULT_MAX_CBOR_DEPTH {
        value = Value::Array(vec![value]);
    }
    let err = encode_cbor(&value, &CborOptions::default()).unwrap_err();
    assert!(err.0.contains("depth"));
}

#[test]
fn enforces_declared_length_limits() {
    let length = DEFAULT_MAX_CBOR_BYTE_LENGTH + 1;
    let bytes_trunc = format!("5a{:08x}", length);
    let text_trunc = format!("7a{:08x}", length);
    let array_trunc = format!("9a{:08x}", DEFAULT_MAX_CBOR_CONTAINER_LENGTH + 1);
    let map_trunc = format!("ba{:08x}", DEFAULT_MAX_CBOR_CONTAINER_LENGTH + 1);
    for hex_str in [bytes_trunc, text_trunc, array_trunc, map_trunc] {
        let err = decode_cbor(&from_hex(&hex_str), &CborOptions::default()).unwrap_err();
        assert!(err.0.contains("limit"), "expected limit error for {hex_str}");
    }
}

#[test]
fn supports_stricter_caller_provided_limits() {
    assert!(decode_cbor(
        &from_hex("83010203"),
        &CborOptions { max_container_length: Some(2), ..Default::default() }
    ).is_err());
    assert!(decode_cbor(
        &from_hex("626162"),
        &CborOptions { max_byte_length: Some(2), ..Default::default() }
    ).is_err());
    assert!(encode_cbor(
        &Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        &CborOptions { max_container_length: Some(2), ..Default::default() }
    ).is_err());
    assert!(encode_cbor(
        &Value::Text("ab".into()),
        &CborOptions { max_byte_length: Some(2), ..Default::default() }
    ).is_err());
}
