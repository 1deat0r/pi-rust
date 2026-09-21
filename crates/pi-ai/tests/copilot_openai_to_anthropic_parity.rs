#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `upstream_pi/packages/ai/test/transform-messages-copilot-openai-to-anthropic.test.ts`.
//!
//! A Copilot Claude session (anthropic-messages lane) replays an OpenAI-lane
//! transcript. `transform_messages` with the Anthropic ID normalizer must:
//! convert thinking to text on model change, strip tool-call thought
//! signatures on migration, normalize `|` IDs, and synthesize results only
//! for still-missing trailing calls.

use pi_ai::api::transform_messages::transform_messages;
use pi_ai::{
    AssistantMessage, ContentBlock, Message, Model, ModelInput, ToolResultMessage, UserContent,
};

fn copilot_claude_model() -> Model {
    let mut model = Model::new(
        "claude-sonnet-4.6",
        "Claude Sonnet 4.6",
        "anthropic-messages",
        "github-copilot",
    );
    model.input = vec![ModelInput::Text, ModelInput::Image];
    model
}

/// Upstream `normalizeToolCallId` from anthropic-messages.ts: non-`[a-zA-Z0-9_-]`
/// become `_`, truncated to 64 chars.
fn anthropic_normalize(id: &str, _m: &Model, _s: &AssistantMessage) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn openai_assistant(blocks: Vec<ContentBlock>, model_id: &str) -> Message {
    let mut assistant = AssistantMessage::new();
    assistant.set_api_provider_model("openai-responses", "github-copilot", model_id);
    assistant.set_stop_reason(pi_ai::StopReason::ToolUse);
    assistant.set_content(blocks);
    Message::Assistant(assistant)
}

#[test]
fn migration_converts_thinking_to_plain_text_when_source_model_differs() {
    let messages = vec![
        Message::User(UserContent::string("hello", 1)),
        openai_assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "Let me think about this...".to_string(),
                    thinking_signature: Some("reasoning_content".to_string()),
                    redacted: None,
                },
                ContentBlock::text("Hi there!"),
            ],
            "gpt-4o",
        ),
    ];

    let result = transform_messages(
        &messages,
        &copilot_claude_model(),
        Some(&anthropic_normalize),
    );
    let Message::Assistant(assistant) = result
        .iter()
        .find(|m| matches!(m, Message::Assistant(_)))
        .expect("assistant survives migration")
    else {
        panic!("expected assistant message")
    };
    assert!(
        !assistant
            .content()
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "no thinking blocks survive a cross-model migration"
    );
    let texts: Vec<&str> = assistant
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.len() >= 2,
        "converted thinking plus original text survive, got {texts:?}"
    );
}

#[test]
fn migration_strips_thought_signature_from_tool_calls() {
    let migrated_call = ContentBlock::ToolCall {
        id: "call_123".to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({"command": "ls"}),
        thought_signature: Some(r#"{"type":"reasoning.encrypted"}"#.to_string()),
        namespace: None,
    };
    let messages = vec![
        Message::User(UserContent::string("run a command", 1)),
        openai_assistant(vec![migrated_call], "gpt-5"),
        Message::ToolResult(ToolResultMessage::text("call_123", "bash", "output", false)),
    ];

    let result = transform_messages(
        &messages,
        &copilot_claude_model(),
        Some(&anthropic_normalize),
    );
    let Message::Assistant(assistant) = result
        .iter()
        .find(|m| matches!(m, Message::Assistant(_)))
        .expect("assistant survives migration")
    else {
        panic!("expected assistant message")
    };
    match &assistant.content()[0] {
        ContentBlock::ToolCall {
            thought_signature: None,
            ..
        } => {}
        other => panic!("thoughtSignature must be stripped on migration, got {other:?}"),
    }
}

#[test]
fn migration_adds_synthetic_result_for_trailing_orphaned_tool_call() {
    let messages = vec![
        Message::User(UserContent::string("read the file", 1)),
        openai_assistant(
            vec![ContentBlock::tool_call(
                "call_123|fc_123",
                "read",
                serde_json::json!({"path": "README.md"}),
            )],
            "gpt-5",
        ),
    ];

    let result = transform_messages(
        &messages,
        &copilot_claude_model(),
        Some(&anthropic_normalize),
    );
    let Message::ToolResult(last) = result.last().expect("synthetic result appended") else {
        panic!("expected trailing synthetic tool result")
    };
    assert_eq!(last.tool_call_id(), "call_123_fc_123");
    assert_eq!(last.tool_name(), "read");
    assert!(last.is_error());
    assert!(
        matches!(last.content().first(), Some(ContentBlock::Text { text, .. }) if text == "No result provided"),
        "synthetic result carries the upstream diagnostic"
    );
}

#[test]
fn migration_synthesizes_only_still_missing_trailing_results() {
    let messages = vec![
        Message::User(UserContent::string("run commands", 1)),
        openai_assistant(
            vec![
                ContentBlock::tool_call(
                    "call_1|fc_1",
                    "read",
                    serde_json::json!({"path": "README.md"}),
                ),
                ContentBlock::tool_call(
                    "call_2|fc_2",
                    "bash",
                    serde_json::json!({"command": "pwd"}),
                ),
            ],
            "gpt-5",
        ),
        Message::ToolResult(ToolResultMessage::text(
            "call_1|fc_1",
            "read",
            "done",
            false,
        )),
    ];

    let result = transform_messages(
        &messages,
        &copilot_claude_model(),
        Some(&anthropic_normalize),
    );
    let synthetics: Vec<&ToolResultMessage> = result
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) if r.is_error() => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(synthetics.len(), 1, "only the missing call is synthesized");
    assert_eq!(synthetics[0].tool_call_id(), "call_2_fc_2");
    assert_eq!(synthetics[0].tool_name(), "bash");
}
