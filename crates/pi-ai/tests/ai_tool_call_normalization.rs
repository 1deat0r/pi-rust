#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use pi_ai::api::transform_messages::transform_messages;
use pi_ai::{AssistantMessage, ContentBlock, Message, Model, StopReason, ToolResultMessage};
use serde_json::{json, Value};

fn target_model() -> Model {
    Model::new("claude-sonnet", "Claude", "anthropic-messages", "anthropic")
}

fn assistant(
    api: &str,
    provider: &str,
    model: &str,
    content: Vec<ContentBlock>,
) -> AssistantMessage {
    let mut message = AssistantMessage::new().with_timestamp(7);
    message.set_api_provider_model(api, provider, model);
    message.set_content(content);
    message.set_stop_reason(StopReason::ToolUse);
    message
}

fn normalize(id: &str, _: &Model, _: &AssistantMessage) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[test]
fn cross_provider_ids_results_and_opaque_metadata_follow_one_normalization_map() {
    let call = ContentBlock::ToolCall {
        id: "call|provider:opaque".into(),
        name: "read".into(),
        arguments: Value::String("malformed-but-preserved".into()),
        thought_signature: Some("encrypted-reasoning".into()),
        namespace: Some("extension.tools".into()),
    };
    let source = assistant("openai-responses", "openai", "gpt-5", vec![call]);
    let result = ToolResultMessage::text("call|provider:opaque", "read", "ok", false)
        .with_details_usage_timestamp(None, Some(json!({"provider":"opaque"})), 9);

    let transformed = transform_messages(
        &[Message::Assistant(source), Message::ToolResult(result)],
        &target_model(),
        Some(&normalize),
    );

    assert_eq!(transformed.len(), 2);
    let Message::Assistant(message) = &transformed[0] else {
        panic!("assistant");
    };
    let ContentBlock::ToolCall {
        id,
        arguments,
        thought_signature,
        namespace,
        ..
    } = &message.content()[0]
    else {
        panic!("tool call");
    };
    assert_eq!(id, "call_provider_opaque");
    assert_eq!(arguments, "malformed-but-preserved");
    assert_eq!(thought_signature, &None);
    assert_eq!(namespace.as_deref(), Some("extension.tools"));

    let Message::ToolResult(result) = &transformed[1] else {
        panic!("tool result");
    };
    assert_eq!(result.tool_call_id(), "call_provider_opaque");
    assert_eq!(result.details(), Some(&json!({"provider":"opaque"})));
    assert_eq!(result.timestamp(), 9);
}

#[test]
fn same_model_replay_preserves_ids_signatures_namespace_and_arguments() {
    let call = ContentBlock::ToolCall {
        id: "same|opaque".into(),
        name: "bash".into(),
        arguments: Value::Null,
        thought_signature: Some("same-model-signature".into()),
        namespace: Some("native".into()),
    };
    let source = assistant(
        "anthropic-messages",
        "anthropic",
        "claude-sonnet",
        vec![call.clone()],
    );
    let transformed = transform_messages(
        &[Message::Assistant(source)],
        &target_model(),
        Some(&normalize),
    );

    let Message::Assistant(message) = &transformed[0] else {
        panic!("assistant");
    };
    assert_eq!(message.content()[0], call);
    let Message::ToolResult(synthetic) = &transformed[1] else {
        panic!("synthetic result");
    };
    assert_eq!(synthetic.tool_call_id(), "same|opaque");
    assert!(synthetic.is_error());
}

#[test]
fn duplicate_calls_match_upstream_missing_result_cleanup() {
    let source = assistant(
        "openai-responses",
        "openai",
        "gpt-5",
        vec![
            ContentBlock::tool_call("dup|1", "read", json!({"path":"a"})),
            ContentBlock::tool_call("dup|1", "read", json!({"path":"b"})),
            ContentBlock::tool_call("missing|2", "bash", json!({"command":"pwd"})),
        ],
    );
    let supplied = ToolResultMessage::text("dup|1", "read", "done", false);
    let transformed = transform_messages(
        &[Message::Assistant(source), Message::ToolResult(supplied)],
        &target_model(),
        Some(&normalize),
    );

    let results: Vec<&ToolResultMessage> = transformed
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_call_id(), "dup_1");
    assert!(!results[0].is_error());
    assert_eq!(results[1].tool_call_id(), "missing_2");
    assert!(results[1].is_error());
}

#[test]
fn failed_turn_is_dropped_after_prior_orphans_are_closed() {
    let pending = assistant(
        "openai-responses",
        "openai",
        "gpt-5",
        vec![ContentBlock::tool_call("orphan|1", "read", json!({}))],
    );
    let mut failed = assistant(
        "openai-responses",
        "openai",
        "gpt-5",
        vec![ContentBlock::tool_call("partial|2", "bash", json!({}))],
    );
    failed.set_stop_reason(StopReason::Error);

    let transformed = transform_messages(
        &[Message::Assistant(pending), Message::Assistant(failed)],
        &target_model(),
        Some(&normalize),
    );
    assert_eq!(transformed.len(), 2);
    assert!(matches!(transformed[0], Message::Assistant(_)));
    assert!(matches!(
        &transformed[1],
        Message::ToolResult(result)
            if result.tool_call_id() == "orphan_1" && result.is_error()
    ));
}
