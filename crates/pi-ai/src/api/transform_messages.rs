//! Message transformation for provider payloads — port of
//! `packages/ai/src/api/transform-messages.ts`.
//!
//! Converts a unified message history into a safe replay history for a
//! specific model: downgrades unsupported images to placeholders, drops or
//! converts cross-model thinking blocks, strips cross-model thought
//! signatures, and normalizes tool-call IDs (OpenAI Responses can emit
//! 450+ char IDs with `|`; Anthropic/Google require `^[a-zA-Z0-9_-]+$`).

use crate::model::Model;
use crate::types::{
    AssistantMessage, ContentBlock, Message, ToolResultMessage, UserContent, UserContentBody,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

/// Replace image blocks with a placeholder, collapsing consecutive
/// placeholders (upstream `replaceImagesWithPlaceholder`).
pub fn replace_images_with_placeholder(
    blocks: &[ContentBlock],
    placeholder: &str,
) -> Vec<ContentBlock> {
    let mut result: Vec<ContentBlock> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in blocks {
        match block {
            ContentBlock::Image { .. } => {
                if !previous_was_placeholder {
                    result.push(ContentBlock::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            _ => {
                let is_placeholder = match block {
                    ContentBlock::Text { text, .. } => text == placeholder,
                    _ => false,
                };
                result.push(block.clone());
                previous_was_placeholder = is_placeholder;
            }
        }
    }
    result
}

fn downgrade_unsupported_images(model: &Model, messages: Vec<Message>) -> Vec<Message> {
    if model.input.contains(&crate::model::ModelInput::Image) {
        return messages;
    }
    messages
        .into_iter()
        .map(|msg| match msg {
            Message::User(UserContent::RoleUser {
                content: UserContentBody::Blocks(blocks),
                timestamp,
            }) => Message::User(UserContent::RoleUser {
                content: UserContentBody::Blocks(replace_images_with_placeholder(
                    &blocks,
                    NON_VISION_USER_IMAGE_PLACEHOLDER,
                )),
                timestamp,
            }),
            Message::ToolResult(r) => Message::ToolResult(replace_tool_result_images(
                r,
                NON_VISION_TOOL_IMAGE_PLACEHOLDER,
            )),
            other => other,
        })
        .collect()
}

fn replace_tool_result_images(r: ToolResultMessage, placeholder: &str) -> ToolResultMessage {
    let ToolResultMessage::ToolResult {
        tool_call_id,
        tool_name,
        content,
        details,
        usage,
        added_tool_names,
        is_error,
        timestamp,
    } = r;
    ToolResultMessage::ToolResult {
        tool_call_id,
        tool_name,
        content: replace_images_with_placeholder(&content, placeholder),
        details,
        usage,
        added_tool_names,
        is_error,
        timestamp,
    }
}

fn insert_synthetic_tool_results(
    result: &mut Vec<Message>,
    pending_tool_calls: &mut Vec<(String, String)>,
    existing_tool_result_ids: &mut std::collections::HashSet<String>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }

    for (tool_call_id, tool_name) in pending_tool_calls.drain(..) {
        if !existing_tool_result_ids.contains(&tool_call_id) {
            result.push(Message::ToolResult(ToolResultMessage::text(
                tool_call_id,
                tool_name,
                "No result provided",
                true,
            )));
        }
    }
    existing_tool_result_ids.clear();
}

/// Normalize tool call ID for cross-provider compatibility.
///
/// `normalize_tool_call_id` is called only for cross-model tool calls when
/// provided. Signature: `(id, model, source) -> normalized id`.
pub fn transform_messages<F>(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&F>,
) -> Vec<Message>
where
    F: Fn(&str, &Model, &AssistantMessage) -> String,
{
    let mut tool_call_id_map: std::collections::HashMap<String, String> = Default::default();
    let image_aware = downgrade_unsupported_images(model, messages.to_vec());

    let mut transformed: Vec<Message> = Vec::with_capacity(image_aware.len());
    for msg in image_aware {
        match msg {
            Message::User(_) => transformed.push(msg),
            Message::ToolResult(r) => {
                let normalized = tool_call_id_map
                    .get(r.tool_call_id())
                    .cloned()
                    .filter(|id| id != r.tool_call_id());
                if let Some(new_id) = normalized {
                    let ToolResultMessage::ToolResult {
                        tool_call_id: _old_id,
                        tool_name,
                        content,
                        details,
                        usage,
                        added_tool_names,
                        is_error,
                        timestamp,
                    } = r;
                    transformed.push(Message::ToolResult(ToolResultMessage::ToolResult {
                        tool_call_id: new_id,
                        tool_name,
                        content,
                        details,
                        usage,
                        added_tool_names,
                        is_error,
                        timestamp,
                    }));
                } else {
                    transformed.push(Message::ToolResult(r));
                }
            }
            Message::Assistant(assistant) => {
                let is_same_model = assistant.provider() == Some(&model.provider)
                    && assistant.api() == Some(&model.api)
                    && assistant.model() == Some(&model.id);

                let mut new_content: Vec<ContentBlock> = Vec::new();
                for block in assistant.content() {
                    match block {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted == Some(true) {
                                // Opaque encrypted content is only valid for the same model.
                                if is_same_model {
                                    new_content.push(block.clone());
                                }
                                continue;
                            }
                            let has_signature =
                                thinking_signature.as_ref().is_some_and(|s| !s.is_empty());
                            if is_same_model && has_signature {
                                // Keep signature-bearing thinking even when empty (encrypted reasoning).
                                new_content.push(block.clone());
                                continue;
                            }
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            if is_same_model {
                                new_content.push(block.clone());
                            } else {
                                // Cross-model: convert thinking to plain text.
                                new_content.push(ContentBlock::text(thinking.clone()));
                            }
                        }
                        ContentBlock::Text { text, .. } => {
                            if is_same_model {
                                new_content.push(block.clone());
                            } else {
                                new_content.push(ContentBlock::text(text.clone()));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            thought_signature,
                            ..
                        } => {
                            let mut tool_call = block.clone();
                            if !is_same_model && thought_signature.is_some() {
                                tool_call.clear_thought_signature();
                            }
                            if !is_same_model {
                                if let Some(f) = normalize_tool_call_id {
                                    let new_id = f(id, model, &assistant);
                                    if new_id != *id {
                                        tool_call_id_map.insert(id.clone(), new_id.clone());
                                        tool_call.set_tool_call_id(new_id);
                                    }
                                }
                            }
                            new_content.push(tool_call);
                        }
                        _ => new_content.push(block.clone()),
                    }
                }

                let AssistantMessage::Assistant {
                    api,
                    provider,
                    model: m,
                    response_model,
                    response_id,
                    diagnostics,
                    usage,
                    stop_reason,
                    deferred,
                    error_message,
                    raw_stop_reason,
                    end_turn,
                    timestamp,
                    ..
                } = &assistant;
                let mut rebuilt = AssistantMessage::new();
                let AssistantMessage::Assistant {
                    api: rapi,
                    provider: rprovider,
                    model: rmodel,
                    response_model: rrm,
                    response_id: rrid,
                    diagnostics: rdiagnostics,
                    usage: rusage,
                    stop_reason: rstop,
                    deferred: rdeferred,
                    error_message: rerr,
                    raw_stop_reason: rraw,
                    end_turn: rend,
                    timestamp: rts,
                    content: rcontent,
                } = &mut rebuilt;
                *rapi = api.clone();
                *rprovider = provider.clone();
                *rmodel = m.clone();
                *rrm = response_model.clone();
                *rrid = response_id.clone();
                *rdiagnostics = diagnostics.clone();
                *rusage = usage.clone();
                *rstop = *stop_reason;
                *rdeferred = deferred.clone();
                *rerr = error_message.clone();
                *rraw = raw_stop_reason.clone();
                *rend = *end_turn;
                *rts = *timestamp;
                *rcontent = new_content;
                transformed.push(Message::Assistant(rebuilt));
            }
        }
    }
    // Complete the replay pass just like upstream `transformMessages`: an
    // interrupted tool-call turn must have a result for every unresolved call,
    // while errored/aborted assistant messages are not replayable turns.
    let mut result = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls = Vec::new();
    let mut existing_tool_result_ids = std::collections::HashSet::new();

    for msg in transformed {
        match msg {
            Message::Assistant(assistant) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if matches!(
                    assistant.stop_reason(),
                    Some(crate::types::StopReason::Error | crate::types::StopReason::Aborted)
                ) {
                    continue;
                }
                pending_tool_calls = assistant
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                if !pending_tool_calls.is_empty() {
                    existing_tool_result_ids.clear();
                }
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id().to_string());
                result.push(Message::ToolResult(tool_result));
            }
            Message::User(user) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(Message::User(user));
            }
        }
    }
    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::{Model, ModelInput};
    use crate::types::*;

    fn model_with(reasoning: bool) -> Model {
        let mut m = Model::new("gemini-3-pro", "G", "google-generative-ai", "google");
        m.reasoning = reasoning;
        m.input = vec![ModelInput::Text, ModelInput::Image];
        m
    }

    fn text_msg(s: &str) -> Message {
        Message::User(UserContent::string(s, 1))
    }

    fn blocks_msg(blocks: Vec<ContentBlock>) -> Message {
        Message::User(UserContent::blocks(blocks, 2))
    }

    fn assistant_msg(blocks: Vec<ContentBlock>) -> AssistantMessage {
        let mut m = AssistantMessage::new();
        *m.content_mut() = blocks;
        m
    }

    #[test]
    fn user_messages_pass_through() {
        let model = model_with(true);
        let msgs = vec![text_msg("hi"), text_msg("there")];
        let out = transform_messages(
            &msgs,
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn non_vision_model_replaces_images() {
        let mut model = model_with(true);
        model.input = vec![ModelInput::Text];
        let msgs = vec![
            text_msg("hi"),
            blocks_msg(vec![
                ContentBlock::image("aGVsbG8=", "image/png"),
                ContentBlock::text("after"),
            ]),
        ];
        let out = transform_messages(
            &msgs,
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        match &out[1] {
            Message::User(UserContent::RoleUser {
                content: UserContentBody::Blocks(b),
                ..
            }) => {
                assert_eq!(b.len(), 2);
                match &b[0] {
                    ContentBlock::Text { text, .. } => {
                        assert_eq!(text, NON_VISION_USER_IMAGE_PLACEHOLDER)
                    }
                    _ => panic!("expected text"),
                }
                match &b[1] {
                    ContentBlock::Text { text, .. } => assert_eq!(text, "after"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn thinking_cross_model_becomes_text() {
        let model = model_with(true);
        let mut a = assistant_msg(vec![ContentBlock::thinking("reasoning here")]);
        a.set_api_provider_model("google-generative-ai", "google", "other-model");
        let msgs = vec![Message::Assistant(a.clone())];
        let out = transform_messages(
            &msgs,
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        match &out[0] {
            Message::Assistant(x) => match &x.content()[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, "reasoning here"),
                c => panic!("expected text, got {c:?}"),
            },
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn text_signature_is_kept_same_model_and_stripped_cross_model() {
        let model = model_with(true);
        let signed_text = ContentBlock::Text {
            text: "signed".to_string(),
            text_signature: Some("provider-opaque".to_string()),
        };

        let mut same = assistant_msg(vec![signed_text.clone()]);
        same.set_api_provider_model("google-generative-ai", "google", "gemini-3-pro");
        let same = transform_messages(
            &[Message::Assistant(same)],
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        assert!(matches!(
            &same[0],
            Message::Assistant(message)
                if matches!(
                    &message.content()[0],
                    ContentBlock::Text { text_signature: Some(signature), .. }
                        if signature == "provider-opaque"
                )
        ));

        let mut foreign = assistant_msg(vec![signed_text]);
        foreign.set_api_provider_model("google-generative-ai", "google", "other-model");
        let foreign = transform_messages(
            &[Message::Assistant(foreign)],
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        assert!(matches!(
            &foreign[0],
            Message::Assistant(message)
                if matches!(
                    &message.content()[0],
                    ContentBlock::Text { text, text_signature: None }
                        if text == "signed"
                )
        ));
    }

    #[test]
    fn tool_call_id_normalization_maps_tool_results() {
        let model = model_with(true);
        let mut a = assistant_msg(vec![ContentBlock::tool_call(
            "bad|id-with|specials".to_string(),
            "bash",
            serde_json::json!({}),
        )]);
        a.set_api_provider_model("openai-responses", "openai", "gpt-5");

        let normalize = |id: &str, _m: &Model, _s: &AssistantMessage| {
            id.replace(
                |c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-',
                "_",
            )
        };
        let msgs = vec![
            Message::Assistant(a.clone()),
            Message::ToolResult(ToolResultMessage::new(
                "bad|id-with|specials".to_string(),
                "bash",
                vec![ContentBlock::text("ok")],
                false,
            )),
        ];
        let out = transform_messages(&msgs, &model, Some(&normalize));
        assert_eq!(out.len(), 2);
        match &out[0] {
            Message::Assistant(x) => match &x.content()[0] {
                ContentBlock::ToolCall { id, .. } => {
                    assert_eq!(id, "bad_id-with_specials");
                }
                c => panic!("expected toolCall: {c:?}"),
            },
            _ => panic!("expected assistant"),
        }
        match &out[1] {
            Message::ToolResult(t) => {
                assert_eq!(t.tool_call_id(), "bad_id-with_specials");
            }
            _ => panic!("expected tool result"),
        }
    }

    #[test]
    fn replay_inserts_only_missing_tool_results_and_drops_failed_turns() {
        let model = model_with(true);
        let assistant = assistant_msg(vec![
            ContentBlock::tool_call("call-1", "lookup", serde_json::json!({})),
            ContentBlock::tool_call("call-2", "read", serde_json::json!({})),
        ]);
        let mut failed = assistant_msg(vec![ContentBlock::text("partial")]);
        failed.set_stop_reason(crate::types::StopReason::Error);

        let out = transform_messages(
            &[
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage::text("call-1", "lookup", "found", false)),
                Message::Assistant(failed),
            ],
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );

        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Message::Assistant(_)));
        assert!(matches!(
            &out[1],
            Message::ToolResult(result) if result.tool_call_id() == "call-1" && !result.is_error()
        ));
        assert!(matches!(
            &out[2],
            Message::ToolResult(result)
                if result.tool_call_id() == "call-2"
                    && result.tool_name() == "read"
                    && result.is_error()
                    && matches!(result.content().first(), Some(ContentBlock::Text { text, .. }) if text == "No result provided")
        ));
    }

    #[test]
    fn redacted_thinking_dropped_cross_model_kept_same_model() {
        let model = model_with(true);
        // Cross-model redacted thinking is dropped entirely.
        let mut a = assistant_msg(vec![ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: Some("sig".into()),
            redacted: Some(true),
        }]);
        a.set_api_provider_model("google-generative-ai", "google", "other-model");
        let out = transform_messages(
            &[Message::Assistant(a)],
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        match &out[0] {
            Message::Assistant(x) => assert!(x.content().is_empty()),
            _ => panic!(),
        }
        // Same model: kept.
        let mut a = assistant_msg(vec![ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: Some("sig".into()),
            redacted: Some(true),
        }]);
        a.set_api_provider_model("google-generative-ai", "google", "gemini-3-pro");
        let out = transform_messages(
            &[Message::Assistant(a)],
            &model,
            None::<&fn(&str, &Model, &AssistantMessage) -> String>,
        );
        match &out[0] {
            Message::Assistant(x) => assert_eq!(x.content().len(), 1),
            _ => panic!(),
        }
    }
}
