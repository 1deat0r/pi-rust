//! Port of the behavioral coverage from `packages/protocol/test/protocol.test.ts`.

use pi_protocol::*;

fn empty_server_snapshot() -> ServerSnapshot {
    ServerSnapshot {
        server_id: "server-1".into(),
        protocol_version: PROTOCOL_VERSION,
        revision: 0,
        sessions: vec![],
        models: vec![],
    }
}

fn client_hello() -> ClientMessage {
    ClientMessage::Hello {
        version: PROTOCOL_VERSION,
    }
}

fn server_hello() -> ServerMessage {
    ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        connection_id: "connection-1".into(),
        snapshot: empty_server_snapshot(),
    }
}

fn as_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn uses_protocol_version_1() {
    assert_eq!(PROTOCOL_VERSION, 1);
    assert!(is_supported_protocol_version(1));
    assert!(!is_supported_protocol_version(2));
}

#[test]
fn accepts_integer_client_hello_versions_for_negotiation() {
    for version in [0u64, PROTOCOL_VERSION, PROTOCOL_VERSION + 1] {
        let message = ClientMessage::Hello { version };
        assert_eq!(parse_client_message(&as_json(&message)).unwrap(), message);
    }
}

#[test]
fn rejects_invalid_handshakes() {
    // string version
    let v = serde_json::json!({"type": "hello", "version": String::from("1")});
    assert!(parse_client_message(&v).is_err());
    // fractional version
    let v = serde_json::json!({"type": "hello", "version": 1.5});
    assert!(parse_client_message(&v).is_err());
    // credential field
    let v = serde_json::json!({"type": "hello", "version": 1, "token": "secret"});
    assert!(parse_client_message(&v).is_err());
    // unknown field
    let v = serde_json::json!({"type": "hello", "version": 1, "extra": true});
    assert!(parse_client_message(&v).is_err());
}

#[test]
fn does_not_parse_json_strings_as_wire_messages() {
    let s = serde_json::json!(serde_json::to_string(&client_hello()).unwrap());
    assert!(parse_client_message(&s).is_err());
    let s = serde_json::json!(serde_json::to_string(&server_hello()).unwrap());
    assert!(parse_server_message(&s).is_err());
}

#[test]
fn rejects_image_input_while_mvp_remains_text_only() {
    let v = serde_json::json!({
        "type": "request",
        "id": "request-1",
        "request": {
            "command": "prompt",
            "sessionId": "session-1",
            "text": "inspect",
            "images": [{"type": "image", "data": "abc", "mimeType": "image/png"}],
        },
    });
    assert!(parse_client_message(&v).is_err());
}

#[test]
fn parses_server_handshake_snapshot() {
    assert_eq!(
        parse_server_message(&as_json(&server_hello())).unwrap(),
        server_hello()
    );
}

#[test]
fn encode_client_message_round_trip() {
    let message = ClientMessage::Request {
        id: "request-1".into(),
        request: Command::Prompt {
            session_id: "session-1".into(),
            text: "hello".into(),
        },
    };
    let bytes = encode_client_message(&message, &FrameDecoderOptions::default()).unwrap();
    let mut decoder = ClientMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    let messages = decoder.push(&bytes).unwrap();
    assert_eq!(messages, vec![message]);
    decoder.end().unwrap();
}

#[test]
fn encode_server_message_round_trip() {
    let message = ServerMessage::Response {
        id: "request-1".into(),
        ok: true,
        result: Some(CommandResult::List { sessions: vec![] }),
        error: None,
    };
    let bytes = encode_server_message(&message, &FrameDecoderOptions::default()).unwrap();
    let mut decoder = ServerMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    let messages = decoder.push(&bytes).unwrap();
    assert_eq!(messages, vec![message]);
    decoder.end().unwrap();
}

#[test]
fn incremental_message_decoding_across_chunk_boundaries() {
    let a = ClientMessage::Hello { version: 1 };
    let b = ClientMessage::Request {
        id: "r1".into(),
        request: Command::List,
    };
    let mut wire = encode_client_message(&a, &FrameDecoderOptions::default()).unwrap();
    wire.extend(encode_client_message(&b, &FrameDecoderOptions::default()).unwrap());
    let mut decoder = ClientMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    let mut got = Vec::new();
    for chunk in wire.chunks(3) {
        got.extend(decoder.push(chunk).unwrap());
    }
    assert_eq!(got, vec![a, b]);
    decoder.end().unwrap();
}

#[test]
fn decoder_fails_on_garbage_frame() {
    let mut decoder = ClientMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    // Valid framing (length 4) but invalid CBOR payload.
    let payload = [0xff, 0xff, 0xff, 0xff];
    let mut wire = vec![0, 0, 0, 4];
    wire.extend_from_slice(&payload);
    assert!(decoder.push(&wire).is_err());
}

#[test]
fn decoder_stays_failed_after_error() {
    let mut decoder = ClientMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    // Valid framing (length 4) but invalid CBOR payload.
    let mut wire = vec![0, 0, 0, 4];
    wire.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    assert!(decoder.push(&wire).is_err());
    // subsequent pushes report failed state
    assert!(decoder.push(&[]).is_err());
    assert!(decoder.end().is_err());
}

#[test]
fn partial_header_does_not_error_until_end() {
    let mut decoder = ClientMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    // One header byte is a partial frame: no error on push...
    assert!(decoder.push(&[0x00]).is_ok());
    // ...but end() fails on the partial frame.
    assert!(decoder.end().is_err());
}

#[test]
fn protocol_version_is_literal_integer() {
    let v = serde_json::json!({"type": "hello", "version": "1"});
    assert!(parse_client_message(&v).is_err());
    let v = serde_json::json!({"type": "hello", "version": 1.0});
    // 1.0 in JSON is a float; TypeBox Integer rejects it.
    assert!(parse_client_message(&v).is_err());
}

#[test]
fn command_results_round_trip() {
    let message = ServerMessage::Response {
        id: "r2".into(),
        ok: true,
        result: Some(CommandResult::SetModel {
            session: SessionSnapshot {
                id: "session-1".into(),
                name: None,
                cwd: "/tmp/x".into(),
                created_at: 1,
                updated_at: 2,
                phase: SessionPhase::Idle,
                model: ModelRef {
                    provider: "faux".into(),
                    id: "faux-1".into(),
                },
                thinking_level: ThinkingLevel::Off,
                attached: true,
                locked: false,
                revision: 0,
                transcript: vec![],
                queued_steer: vec![],
                queued_steer_count: 0,
            },
        }),
        error: None,
    };
    let bytes = encode_server_message(&message, &FrameDecoderOptions::default()).unwrap();
    let mut decoder = ServerMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    assert_eq!(decoder.push(&bytes).unwrap(), vec![message]);
    decoder.end().unwrap();
}

#[test]
fn model_metadata_validation() {
    let json = serde_json::json!({
        "provider": "anthropic",
        "id": "claude-sonnet-4",
        "name": "Claude Sonnet 4",
        "api": "anthropic-messages",
        "reasoning": true,
        "input": ["text", "image"],
        "contextWindow": 200000,
        "maxTokens": 64000,
        "cost": {"input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75},
        "supportedThinkingLevels": ["off", "low", "medium", "high"],
        "authenticated": false,
    });
    let parsed = ModelMetadata::parse(&json).unwrap();
    assert_eq!(parsed.api, "anthropic-messages");
    assert_eq!(parsed.supported_thinking_levels.len(), 4);
    assert!(parsed.input.contains(&ModelInput::Image));

    // unknown field rejected
    let mut bad = json.clone();
    bad.as_object_mut()
        .unwrap()
        .insert("extra".into(), serde_json::json!(1));
    assert!(ModelMetadata::parse(&bad).is_err());
    // empty thinking levels rejected
    let mut bad = json.clone();
    bad.as_object_mut()
        .unwrap()
        .insert("supportedThinkingLevels".into(), serde_json::json!([]));
    assert!(ModelMetadata::parse(&bad).is_err());
}
