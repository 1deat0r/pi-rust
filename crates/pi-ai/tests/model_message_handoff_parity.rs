#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_ai::api::anthropic_messages::convert_messages as convert_anthropic_messages;
use pi_ai::api::bedrock_converse::convert_messages as convert_bedrock_messages;
use pi_ai::api::google_shared::convert_messages as convert_google_messages;
use pi_ai::api::openai_completions::{
    convert_messages as convert_completion_messages, OpenAiCompletionsCompat,
};
use pi_ai::api::openai_responses_shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions,
};
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelInput, ToolResultMessage,
    UserContent,
};

fn tool_result_with_image() -> Message {
    Message::ToolResult(ToolResultMessage::new(
        "call-1",
        "read",
        vec![ContentBlock::image("aGk=", "image/png")],
        false,
    ))
}

fn model(id: &str, api: &str, provider: &str, input: Vec<ModelInput>) -> Model {
    let mut model = Model::new(id, id, api, provider);
    model.input = input;
    model
}

#[test]
fn anthropic_image_only_tool_result_keeps_upstream_placeholder() {
    let wire = convert_anthropic_messages(&[tool_result_with_image()], false);
    assert_eq!(wire[0]["role"], "user");
    assert_eq!(wire[0]["content"][0]["type"], "tool_result");
    assert_eq!(
        wire[0]["content"][0]["content"][0],
        serde_json::json!({"type": "text", "text": "(see attached image)"})
    );
    assert_eq!(wire[0]["content"][0]["content"][1]["type"], "image");
}

#[test]
fn openai_completions_image_only_tool_result_keeps_text_and_image_turns() {
    let target = model(
        "gpt-4o",
        "openai-completions",
        "openai",
        vec![ModelInput::Text, ModelInput::Image],
    );
    let compat = OpenAiCompletionsCompat::get(&target);
    let context = Context {
        messages: vec![tool_result_with_image()],
        ..Default::default()
    };
    let wire =
        convert_completion_messages(&target, &context, &compat).expect("completions conversion");
    assert_eq!(wire[0]["role"], "tool");
    assert_eq!(wire[0]["content"], "(see attached image)");
    assert_eq!(wire[1]["role"], "user");
    assert_eq!(
        wire[1]["content"][0]["text"],
        "Attached image(s) from tool result:"
    );
    assert_eq!(wire[1]["content"][1]["type"], "image_url");
}

#[test]
fn bedrock_image_only_tool_result_preserves_image_content() {
    let target = model(
        "anthropic.claude-sonnet-4-5",
        "bedrock-converse-stream",
        "amazon-bedrock",
        vec![ModelInput::Text, ModelInput::Image],
    );
    let context = Context {
        messages: vec![tool_result_with_image()],
        ..Default::default()
    };
    let wire = convert_bedrock_messages(&context, &target, "none", None);
    assert_eq!(wire[0]["role"], "user");
    assert_eq!(wire[0]["content"][0]["toolResult"]["toolUseId"], "call-1");
    assert_eq!(
        wire[0]["content"][0]["toolResult"]["content"][0]["image"]["format"],
        "png"
    );
}

#[test]
fn provider_handoff_preserves_tool_pairing_and_downgrades_images() {
    let mut assistant = AssistantMessage::new();
    assistant.set_api_provider_model("openai-responses", "openai", "source-model");
    assistant.set_stop_reason(pi_ai::StopReason::ToolUse);
    assistant.set_content(vec![ContentBlock::tool_call(
        "foreign|long/item",
        "read",
        serde_json::json!({"path": "a.txt"}),
    )]);
    let messages = vec![
        Message::User(UserContent::string("read it", 1)),
        Message::Assistant(assistant),
        Message::ToolResult(ToolResultMessage::text(
            "foreign|long/item",
            "read",
            "done",
            false,
        )),
        Message::User(UserContent::blocks(
            vec![ContentBlock::image("aGk=", "image/png")],
            2,
        )),
    ];
    let target = model(
        "target-model",
        "openai-responses",
        "openai",
        vec![ModelInput::Text],
    );
    let context = Context {
        messages,
        ..Default::default()
    };
    let wire = convert_responses_messages(
        &target,
        &context,
        &["openai"],
        &ConvertResponsesMessagesOptions::default(),
    )
    .expect("responses conversion");
    assert_eq!(wire[1]["type"], "function_call");
    assert_eq!(wire[1]["call_id"], "foreign");
    assert_eq!(wire[2]["type"], "function_call_output");
    assert_eq!(wire[2]["call_id"], "foreign");
    assert_eq!(wire[3]["role"], "user");
    assert_eq!(wire[3]["content"][0]["type"], "input_text");
    assert_eq!(
        wire[3]["content"][0]["text"],
        "(image omitted: model does not support images)"
    );
}

#[test]
fn google_handoff_keeps_signed_empty_reasoning_only_for_same_model() {
    let mut assistant = AssistantMessage::new();
    assistant.set_api_provider_model("google-generative-ai", "google", "gemini-3-pro");
    assistant.set_content(vec![ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some("c2ln".to_string()),
        redacted: None,
    }]);
    let same = model(
        "gemini-3-pro",
        "google-generative-ai",
        "google",
        vec![ModelInput::Text],
    );
    let foreign = model(
        "other-model",
        "google-generative-ai",
        "google",
        vec![ModelInput::Text],
    );
    let context = Context {
        messages: vec![Message::Assistant(assistant)],
        ..Default::default()
    };
    let same_wire = convert_google_messages(&same, &context);
    assert_eq!(same_wire[0]["parts"][0]["thoughtSignature"], "c2ln");
    assert!(convert_google_messages(&foreign, &context).is_empty());
}
