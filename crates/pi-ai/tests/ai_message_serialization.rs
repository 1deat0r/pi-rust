#![allow(clippy::expect_used, clippy::panic)]

use pi_ai::api::transform_messages::transform_messages;
use pi_ai::{Message, Model, ModelInput};
use serde_json::{json, Value};

fn model(provider: &str, api: &str, id: &str, image: bool) -> Model {
    let mut model = Model::new(id, "Serialization fixture", api, provider);
    model.input = if image {
        vec![ModelInput::Text, ModelInput::Image]
    } else {
        vec![ModelInput::Text]
    };
    model
}

fn comprehensive_history() -> Vec<Message> {
    serde_json::from_value(json!([
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "hello", "textSignature": "user-text-signature"},
                {"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"}
            ],
            "timestamp": 101,
            "unknownUserField": "ignored"
        },
        {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "visible", "textSignature": "text-signature"},
                {
                    "type": "thinking",
                    "thinking": "reasoning",
                    "thinkingSignature": "thinking-signature",
                    "redacted": false
                },
                {
                    "type": "thinking",
                    "thinking": "",
                    "thinkingSignature": "encrypted-redacted-reasoning",
                    "redacted": true
                },
                {
                    "type": "toolCall",
                    "id": "call|foreign",
                    "name": "lookup",
                    "arguments": {"nested": [1, {"ok": true}]},
                    "thoughtSignature": "tool-signature",
                    "namespace": "remote"
                }
            ],
            "api": "openai-responses",
            "provider": "source-provider",
            "model": "source-model",
            "responseModel": "source-model-2026-08-31",
            "responseId": "response-1",
            "diagnostics": [{
                "type": "provider_recovery",
                "timestamp": 102,
                "details": {"attempt": 2}
            }],
            "usage": {
                "input": 10,
                "output": 5,
                "cacheRead": 3,
                "cacheWrite": 2,
                "cacheWrite1h": 1,
                "reasoning": 4,
                "totalTokens": 20,
                "cost": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4, "total": 10}
            },
            "stopReason": "toolUse",
            "rawStopReason": "tool_use",
            "endTurn": false,
            "timestamp": 103,
            "unknownAssistantField": {"ignored": true}
        },
        {
            "role": "toolResult",
            "toolCallId": "call|foreign",
            "toolName": "lookup",
            "content": [
                {"type": "text", "text": "result"},
                {"type": "image", "data": "cmVzdWx0", "mimeType": "image/jpeg"}
            ],
            "details": {"custom": {"deep": ["value"]}},
            "usage": {
                "input": 1,
                "output": 2,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 3,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
            },
            "addedToolNames": ["later_tool"],
            "isError": false,
            "timestamp": 104,
            "unknownToolField": 7
        }
    ]))
    .expect("comprehensive message history should deserialize")
}

#[test]
fn same_model_round_trip_preserves_known_message_metadata_and_custom_tool_details() {
    let history = comprehensive_history();
    let target = model("source-provider", "openai-responses", "source-model", true);

    let transformed = transform_messages(
        &history,
        &target,
        None::<&fn(&str, &Model, &pi_ai::AssistantMessage) -> String>,
    );
    let wire = serde_json::to_value(&transformed).expect("transformed history should serialize");

    assert_eq!(wire[0]["timestamp"], 101);
    assert_eq!(
        wire[0]["content"][0]["textSignature"],
        "user-text-signature"
    );
    assert_eq!(wire[1]["timestamp"], 103);
    assert_eq!(wire[1]["content"][0]["textSignature"], "text-signature");
    assert_eq!(
        wire[1]["content"][1]["thinkingSignature"],
        "thinking-signature"
    );
    assert_eq!(wire[1]["content"][2]["redacted"], true);
    assert_eq!(wire[1]["content"][3]["thoughtSignature"], "tool-signature");
    assert_eq!(wire[1]["content"][3]["namespace"], "remote");
    assert_eq!(wire[1]["responseModel"], "source-model-2026-08-31");
    assert_eq!(wire[1]["responseId"], "response-1");
    assert_eq!(wire[1]["diagnostics"][0]["details"]["attempt"], 2);
    assert_eq!(wire[1]["usage"]["cacheWrite1h"], 1);
    assert_eq!(wire[1]["rawStopReason"], "tool_use");
    assert_eq!(wire[1]["endTurn"], false);
    assert_eq!(wire[2]["timestamp"], 104);
    assert_eq!(wire[2]["details"]["custom"]["deep"], json!(["value"]));
    assert_eq!(wire[2]["addedToolNames"], json!(["later_tool"]));
    assert_eq!(wire[2]["usage"]["totalTokens"], 3);

    for (index, field) in [
        (0, "unknownUserField"),
        (1, "unknownAssistantField"),
        (2, "unknownToolField"),
    ] {
        assert_eq!(
            wire[index].get(field),
            None,
            "unknown fields are ignored safely"
        );
    }
}

#[test]
fn cross_model_replay_normalizes_provider_opaque_fields_but_preserves_pairing_and_timestamps() {
    let history = comprehensive_history();
    let target = model(
        "target-provider",
        "anthropic-messages",
        "target-model",
        true,
    );
    let normalize = |_id: &str, _model: &Model, _source: &pi_ai::AssistantMessage| {
        "normalized_call".to_string()
    };

    let transformed = transform_messages(&history, &target, Some(&normalize));
    let wire = serde_json::to_value(&transformed).expect("cross-model history should serialize");

    assert_eq!(wire[0]["timestamp"], 101);
    assert_eq!(wire[1]["timestamp"], 103);
    assert_eq!(wire[2]["timestamp"], 104);
    assert_eq!(
        wire[1]["content"][0],
        json!({"type": "text", "text": "visible"})
    );
    assert_eq!(
        wire[1]["content"][1],
        json!({"type": "text", "text": "reasoning"})
    );
    assert_eq!(
        wire[1]["content"].as_array().expect("content array").len(),
        3
    );
    assert_eq!(wire[1]["content"][2]["id"], "normalized_call");
    assert_eq!(wire[1]["content"][2].get("thoughtSignature"), None);
    assert_eq!(wire[1]["content"][2]["namespace"], "remote");
    assert_eq!(wire[2]["toolCallId"], "normalized_call");
    assert_eq!(wire[2]["details"]["custom"]["deep"], json!(["value"]));
}

#[test]
fn malformed_unknown_content_variants_fail_instead_of_becoming_valid_context() {
    let invalid: Result<Message, _> = serde_json::from_value(json!({
        "role": "user",
        "content": [{"type": "futureBlock", "payload": "opaque"}],
        "timestamp": 1
    }));
    assert!(invalid.is_err());

    let invalid_scalar: Result<Message, _> = serde_json::from_value(json!({
        "role": "assistant",
        "content": 17,
        "timestamp": 2
    }));
    assert!(invalid_scalar.is_err());
}

#[test]
fn serialized_history_round_trips_without_losing_known_fields() {
    let history = comprehensive_history();
    let encoded = serde_json::to_string(&history).expect("history should serialize");
    let decoded: Vec<Message> = serde_json::from_str(&encoded).expect("history should deserialize");
    assert_eq!(decoded, history);

    let wire: Value = serde_json::from_str(&encoded).expect("serialized history is JSON");
    assert_eq!(wire[0]["timestamp"], 101);
    assert_eq!(wire[1]["timestamp"], 103);
    assert_eq!(wire[2]["timestamp"], 104);
}
