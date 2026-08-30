#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_ai::api::transform_messages::transform_messages;
use pi_ai::{Message, Model, ModelInput, UserContentBody};

fn text_model() -> Model {
    let mut model = Model::new("fixture-model", "Fixture", "openai-completions", "fixture");
    model.input = vec![ModelInput::Text];
    model
}

#[test]
fn lax_null_and_missing_content_normalize_to_empty_blocks() {
    let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
        {"role": "user", "content": null, "timestamp": 1},
        {"role": "assistant", "content": null, "timestamp": 2},
        {
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "fixture",
            "isError": false,
            "timestamp": 3
        }
    ]))
    .expect("pinned lax-message-content fixture should deserialize");

    let transformed = transform_messages(
        &messages,
        &text_model(),
        None::<&fn(&str, &Model, &pi_ai::AssistantMessage) -> String>,
    );

    assert_eq!(transformed.len(), 3);
    match &transformed[0] {
        Message::User(user) => {
            assert!(matches!(user.content(), UserContentBody::Blocks(blocks) if blocks.is_empty()))
        }
        other => panic!("expected user message, got {other:?}"),
    }
    match &transformed[1] {
        Message::Assistant(assistant) => assert!(assistant.content().is_empty()),
        other => panic!("expected assistant message, got {other:?}"),
    }
    match &transformed[2] {
        Message::ToolResult(tool_result) => assert!(tool_result.content().is_empty()),
        other => panic!("expected tool result message, got {other:?}"),
    }
}

#[test]
fn normalized_empty_content_reserializes_as_empty_arrays() {
    let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
        {"role": "user", "timestamp": 1},
        {"role": "assistant", "timestamp": 2},
        {
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "fixture",
            "content": null,
            "isError": false,
            "timestamp": 3
        }
    ]))
    .expect("missing/null content should normalize");

    let wire = serde_json::to_value(messages).expect("normalized messages should serialize");
    assert_eq!(wire[0]["content"], serde_json::json!([]));
    assert_eq!(wire[1]["content"], serde_json::json!([]));
    assert_eq!(wire[2]["content"], serde_json::json!([]));
}

#[test]
fn malformed_scalar_content_remains_rejected() {
    let result = serde_json::from_value::<Message>(serde_json::json!({
        "role": "assistant",
        "content": 7,
        "timestamp": 1
    }));
    assert!(
        result.is_err(),
        "scalar content must not be treated as empty"
    );
}
