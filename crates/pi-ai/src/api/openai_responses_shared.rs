//! Shared utilities for the OpenAI Responses API family — port of
//! `packages/ai/src/api/openai-responses-shared.ts`.
//!
//! Message/tool conversion and the SSE stream processor. Deferred-tools
//! (additional-tools / tool-search placement) and grammar constraints are
//! not yet ported (documented; `deferred` is empty and grammar properties
//! resolve to None), so the standard OpenAI flow is faithful while those
//! paths remain stubs that upstream only exercises for compat providers.

use serde_json::{json, Value};

use crate::model::{calculate_cost, Model};
use crate::partial_json::parse_streaming_json;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, StopReason, Usage,
    UserContent, UserContentBody,
};

use super::openai_completions::short_hash;
use super::transform_messages::transform_messages;

// ---------------------------------------------------------------------------
// Text signatures
// ---------------------------------------------------------------------------

fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    match phase {
        Some(p) => json!({ "v": 1, "id": id, "phase": p }).to_string(),
        None => json!({ "v": 1, "id": id }).to_string(),
    }
}

fn parse_text_signature(signature: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(signature) = signature else {
        return (None, None);
    };
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed.get("v").and_then(|v| v.as_u64()) == Some(1) {
                if let Some(id) = parsed.get("id").and_then(|v| v.as_str()) {
                    let phase = parsed
                        .get("phase")
                        .and_then(|v| v.as_str())
                        .filter(|p| *p == "commentary" || *p == "final_answer")
                        .map(|s| s.to_string());
                    return (Some(id.to_string()), phase);
                }
            }
        }
    }
    (Some(signature.to_string()), None)
}

// ---------------------------------------------------------------------------
// Tool result output
// ---------------------------------------------------------------------------

fn convert_tool_result_output(model: &Model, content: &[ContentBlock]) -> Value {
    let text_result: Vec<&str> = content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text_result = text_result.join("\n");
    let images: Vec<&ContentBlock> = content
        .iter()
        .filter(|c| matches!(c, ContentBlock::Image { .. }))
        .collect();
    let has_text = !text_result.is_empty();

    if images.is_empty() || !model.input.contains(&crate::model::ModelInput::Image) {
        let value = if has_text {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return json!(value);
    }

    let mut output: Vec<Value> = Vec::new();
    if has_text {
        output.push(json!({ "type": "input_text", "text": text_result }));
    }
    for image in images {
        if let ContentBlock::Image {
            data, mime_type, ..
        } = image
        {
            output.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}"),
            }));
        }
    }
    json!(output)
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

pub struct ConvertResponsesMessagesOptions {
    pub include_system_prompt: bool,
}

impl Default for ConvertResponsesMessagesOptions {
    fn default() -> Self {
        Self {
            include_system_prompt: true,
        }
    }
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let normalized: String = if sanitized.len() > 64 {
        sanitized.chars().take(64).collect()
    } else {
        sanitized
    };
    normalized.trim_end_matches('_').to_string()
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", short_hash(item_id));
    if normalized.len() > 64 {
        normalized.chars().take(64).collect()
    } else {
        normalized
    }
}

/// Convert unified messages to the OpenAI Responses `input` array.
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &[&str],
    options: &ConvertResponsesMessagesOptions,
) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();

    let normalize_tool_call_id =
        |id: &str, target_model: &Model, source: &AssistantMessage| -> String {
            let provider = target_model.provider.as_str();
            if !allowed_tool_call_providers.contains(&provider) {
                return normalize_id_part(id);
            }
            if !id.contains('|') {
                return normalize_id_part(id);
            }
            let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
            let normalized_call_id = normalize_id_part(call_id);
            let is_foreign = source.provider() != Some(&target_model.provider)
                || source.api() != Some(&target_model.api);
            let mut normalized_item_id = if is_foreign {
                build_foreign_responses_item_id(item_id)
            } else {
                normalize_id_part(item_id)
            };
            if !normalized_item_id.starts_with("fc_") {
                normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
            }
            format!("{normalized_call_id}|{normalized_item_id}")
        };

    let transformed = transform_messages(&context.messages, model, Some(&normalize_tool_call_id));

    // System prompt: developer role for reasoning models unless compat opts out.
    if options.include_system_prompt {
        if let Some(system_prompt) = &context.system_prompt {
            if !system_prompt.is_empty() {
                let supports_developer = model
                    .compat
                    .as_ref()
                    .and_then(|c| c.get("supportsDeveloperRole"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let role = if model.reasoning && supports_developer {
                    "developer"
                } else {
                    "system"
                };
                messages.push(json!({ "role": role, "content": system_prompt }));
            }
        }
    }

    let mut msg_index = 0usize;
    for msg in transformed {
        match msg {
            Message::User(UserContent::RoleUser { content, .. }) => match content {
                UserContentBody::String(s) => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": s }],
                    }));
                }
                UserContentBody::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            ContentBlock::Text { text, .. } => {
                                json!({ "type": "input_text", "text": text })
                            }
                            ContentBlock::Image {
                                data, mime_type, ..
                            } => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }),
                            _ => json!({}),
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    messages.push(json!({ "role": "user", "content": content }));
                }
            },
            Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let is_same_provider_and_api = assistant.provider() == Some(&model.provider)
                    && assistant.api() == Some(&model.api);
                let is_same_model =
                    is_same_provider_and_api && assistant.model() == Some(&model.id);
                let is_different_model =
                    is_same_provider_and_api && assistant.model() != Some(&model.id);
                let mut text_block_index = 0usize;

                for block in assistant.content() {
                    match block {
                        ContentBlock::Thinking {
                            thinking_signature: Some(sig),
                            ..
                        } => {
                            // Signature carries a ResponseReasoningItem JSON for replay.
                            if let Ok(item) = serde_json::from_str::<Value>(sig) {
                                output.push(item);
                            }
                        }
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            let (parsed_id, phase) =
                                parse_text_signature(text_signature.as_deref());
                            let fallback = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let msg_id = match parsed_id {
                                Some(id) if id.len() <= 64 => id,
                                Some(_) => format!("msg_{}", short_hash(&fallback)),
                                None => fallback,
                            };
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                                "status": "completed",
                                "id": msg_id,
                            });
                            if let Some(p) = phase {
                                item["phase"] = json!(p);
                            }
                            output.push(item);
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            namespace,
                            ..
                        } => {
                            let (call_id, item_id_raw) = id
                                .split_once('|')
                                .map(|(a, b)| (a.to_string(), Some(b.to_string())))
                                .unwrap_or_else(|| (id.clone(), None));
                            let mut item_id = item_id_raw;
                            if (is_different_model
                                && item_id.as_deref().is_some_and(|s| s.starts_with("fc_")))
                                || !item_id.as_deref().is_some_and(|s| s.starts_with("fc_"))
                            {
                                item_id = None;
                            }
                            let mut item = json!({
                                "type": "function_call",
                                "id": item_id,
                                "call_id": call_id,
                                "name": name,
                                "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
                            });
                            if is_same_model {
                                if let Some(ns) = namespace {
                                    item["namespace"] = json!(ns);
                                }
                            }
                            output.push(item);
                        }
                        _ => {}
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            Message::ToolResult(result) => {
                let (call_id, _) = result
                    .tool_call_id()
                    .split_once('|')
                    .map(|(a, b)| (a.to_string(), Some(b.to_string())))
                    .unwrap_or_else(|| (result.tool_call_id().to_string(), None));
                let output = convert_tool_result_output(model, result.content());
                messages.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
        }
        msg_index += 1;
    }
    messages
}

// ---------------------------------------------------------------------------
// Tool conversion
// ---------------------------------------------------------------------------

pub struct ConvertResponsesToolsOptions {
    pub strict: Option<bool>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
}

impl Default for ConvertResponsesToolsOptions {
    fn default() -> Self {
        Self {
            strict: None,
            supports_strict_mode: true,
            supports_openai_grammar_tools: false,
        }
    }
}

fn tool_json_schema_parameters(tool: &crate::types::Tool, strict: bool) -> Value {
    let mut params = tool.parameters.clone();
    if strict && params.is_object() {
        if let Some(obj) = params.as_object_mut() {
            if !obj.contains_key("additionalProperties") {
                obj.insert("additionalProperties".to_string(), json!(false));
            }
        }
    }
    params
}

fn resolve_strict(tool: &crate::types::Tool, supports_strict_mode: bool) -> Option<bool> {
    match &tool.constrained_sampling {
        Some(crate::types::ConstrainedSampling::JsonSchema { strict }) => {
            if supports_strict_mode {
                Some(matches!(strict, crate::types::StrictPreference::Require))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Convert tools to OpenAI Responses `tools` array.
pub fn convert_responses_tools(
    tools: &[crate::types::Tool],
    options: &ConvertResponsesToolsOptions,
) -> Vec<Value> {
    let default_strict = options.strict.unwrap_or(false);
    let mut result: Vec<Value> = Vec::new();
    for tool in tools {
        let constrained = resolve_strict(tool, options.supports_strict_mode);
        let strict = constrained.unwrap_or(default_strict);
        let mut function_tool = json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool_json_schema_parameters(tool, strict),
        });
        if options.supports_strict_mode {
            function_tool["strict"] = json!(strict);
        }
        result.push(function_tool);
    }
    result
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ResponsesSlot {
    Thinking {
        content_index: usize,
    },
    Text {
        content_index: usize,
    },
    ToolCall {
        content_index: usize,
        partial_json: String,
    },
}

#[derive(Default)]
pub struct ProcessResponsesOptions {
    pub service_tier: Option<String>,
}

/// Multiplexed stream processor (port of `processResponsesStream`).
pub fn process_responses_stream(
    events: &[crate::sse::SseEvent],
    output: &mut AssistantMessage,
    push: &mut dyn FnMut(AssistantMessageEvent),
    model: &Model,
    options: &ProcessResponsesOptions,
) -> Result<(), String> {
    let mut saw_terminal_response_event = false;
    let mut output_slots: std::collections::HashMap<usize, ResponsesSlot> = Default::default();
    let mut reasoning_signatures_by_id: std::collections::HashMap<String, ContentBlock> =
        Default::default();

    let apply_message_phase_stop_reason = |item: &Value, output: &mut AssistantMessage| {
        if item.get("type").and_then(|t| t.as_str()) == Some("message")
            && item.get("phase").and_then(|p| p.as_str()) == Some("final_answer")
        {
            output.set_stop_reason(StopReason::Stop);
        }
    };

    for event in events {
        if event.data.trim().is_empty() || event.data == "[DONE]" {
            continue;
        }
        let parsed: Value = serde_json::from_str(&event.data)
            .map_err(|e| format!("Malformed OpenAI Responses stream chunk: {e}"))?;
        let event_type = parsed
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        match event_type.as_str() {
            "response.created" => {
                if let Some(id) = parsed
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(|v| v.as_str())
                {
                    output.set_response_id(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let item = parsed.get("item").cloned().unwrap_or(json!({}));
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "reasoning" => {
                        output.content_mut().push(ContentBlock::thinking(""));
                        let content_index = output.content_len().saturating_sub(1);
                        output_slots
                            .insert(output_index, ResponsesSlot::Thinking { content_index });
                        push(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    "message" => {
                        apply_message_phase_stop_reason(&item, output);
                        output.content_mut().push(ContentBlock::text(""));
                        let content_index = output.content_len().saturating_sub(1);
                        output_slots.insert(output_index, ResponsesSlot::Text { content_index });
                        push(AssistantMessageEvent::TextStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    "function_call" => {
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let id = item
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let block = ContentBlock::ToolCall {
                            id: format!("{call_id}|{id}"),
                            name,
                            arguments: json!({}),
                            thought_signature: None,
                            namespace: None,
                        };
                        output.content_mut().push(block);
                        let content_index = output.content_len().saturating_sub(1);
                        output_slots.insert(
                            output_index,
                            ResponsesSlot::ToolCall {
                                content_index,
                                partial_json: arguments,
                            },
                        );
                        push(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    _ => {}
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = parsed
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(ResponsesSlot::Thinking { content_index }) =
                    output_slots.get(&output_index)
                {
                    let idx = *content_index;
                    if let Some(ContentBlock::Thinking { thinking, .. }) =
                        output.content_mut().get_mut(idx)
                    {
                        *thinking += &delta;
                    }
                    push(AssistantMessageEvent::ThinkingDelta {
                        content_index: idx,
                        delta,
                        partial: output.clone(),
                    });
                }
            }
            "response.reasoning_summary_part.done" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if let Some(ResponsesSlot::Thinking { content_index }) =
                    output_slots.get(&output_index)
                {
                    let idx = *content_index;
                    if let Some(ContentBlock::Thinking { thinking, .. }) =
                        output.content_mut().get_mut(idx)
                    {
                        *thinking += "\n\n";
                    }
                    push(AssistantMessageEvent::ThinkingDelta {
                        content_index: idx,
                        delta: "\n\n".to_string(),
                        partial: output.clone(),
                    });
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = parsed
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(ResponsesSlot::Text { content_index }) = output_slots.get(&output_index)
                {
                    let idx = *content_index;
                    if let Some(ContentBlock::Text { text, .. }) = output.content_mut().get_mut(idx)
                    {
                        *text += &delta;
                    }
                    push(AssistantMessageEvent::TextDelta {
                        content_index: idx,
                        delta,
                        partial: output.clone(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = parsed
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(ResponsesSlot::ToolCall {
                    content_index,
                    partial_json,
                }) = output_slots.get_mut(&output_index)
                {
                    *partial_json += &delta;
                    let args = parse_streaming_json(partial_json);
                    if let Some(ContentBlock::ToolCall { arguments, .. }) =
                        output.content_mut().get_mut(*content_index)
                    {
                        *arguments = args;
                    }
                    push(AssistantMessageEvent::ToolCallDelta {
                        content_index: *content_index,
                        delta,
                        partial: output.clone(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let final_arguments = parsed
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(ResponsesSlot::ToolCall {
                    content_index,
                    partial_json,
                }) = output_slots.get_mut(&output_index)
                {
                    let previous = partial_json.clone();
                    *partial_json = final_arguments.clone();
                    let args = parse_streaming_json(&final_arguments);
                    if let Some(ContentBlock::ToolCall { arguments, .. }) =
                        output.content_mut().get_mut(*content_index)
                    {
                        *arguments = args;
                    }
                    if final_arguments.starts_with(&previous) {
                        let delta = &final_arguments[previous.len()..];
                        if !delta.is_empty() {
                            push(AssistantMessageEvent::ToolCallDelta {
                                content_index: *content_index,
                                delta: delta.to_string(),
                                partial: output.clone(),
                            });
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let output_index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let item = parsed.get("item").cloned().unwrap_or(json!({}));
                apply_message_phase_stop_reason(&item, output);
                let item_type = item
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                match item_type.as_str() {
                    "reasoning" => {
                        if let Some(ResponsesSlot::Thinking { content_index }) =
                            output_slots.get(&output_index)
                        {
                            let idx = *content_index;
                            let summary_text: Vec<String> = item
                                .get("summary")
                                .and_then(|s| s.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|s| {
                                            s.get("text")
                                                .and_then(|t| t.as_str())
                                                .map(|t| t.to_string())
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let content_text: Vec<String> = item
                                .get("content")
                                .and_then(|c| c.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|s| {
                                            s.get("text")
                                                .and_then(|t| t.as_str())
                                                .map(|t| t.to_string())
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let assembled = if !summary_text.is_empty() {
                                summary_text.join("\n\n")
                            } else if !content_text.is_empty() {
                                content_text.join("\n\n")
                            } else {
                                String::new()
                            };
                            if let Some(ContentBlock::Thinking {
                                thinking,
                                thinking_signature,
                                ..
                            }) = output.content_mut().get_mut(idx)
                            {
                                if assembled.is_empty() {
                                    // keep accumulated deltas
                                } else {
                                    *thinking = assembled;
                                }
                                *thinking_signature = Some(item.to_string());
                                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                                    reasoning_signatures_by_id.insert(
                                        id.to_string(),
                                        ContentBlock::Thinking {
                                            thinking: thinking.clone(),
                                            thinking_signature: Some(item.to_string()),
                                            redacted: None,
                                        },
                                    );
                                }
                            }
                            let content = match &output.content()[idx.min(output.content_len() - 1)]
                            {
                                ContentBlock::Thinking { thinking, .. } => thinking.clone(),
                                _ => String::new(),
                            };
                            push(AssistantMessageEvent::ThinkingEnd {
                                content_index: idx,
                                content,
                                partial: output.clone(),
                            });
                            output_slots.remove(&output_index);
                        }
                    }
                    "message" => {
                        if let Some(ResponsesSlot::Text { content_index }) =
                            output_slots.get(&output_index)
                        {
                            let idx = *content_index;
                            let assembled: String = item
                                .get("content")
                                .and_then(|c| c.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|c| {
                                            if c.get("type").and_then(|t| t.as_str())
                                                == Some("output_text")
                                            {
                                                c.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(|t| t.to_string())
                                            } else {
                                                c.get("refusal")
                                                    .and_then(|t| t.as_str())
                                                    .map(|t| t.to_string())
                                            }
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let id = item
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let phase = item
                                .get("phase")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            if let Some(ContentBlock::Text {
                                text,
                                text_signature,
                            }) = output.content_mut().get_mut(idx)
                            {
                                *text = assembled.clone();
                                *text_signature =
                                    Some(encode_text_signature_v1(&id, phase.as_deref()));
                            }
                            push(AssistantMessageEvent::TextEnd {
                                content_index: idx,
                                content: assembled,
                                partial: output.clone(),
                            });
                            output_slots.remove(&output_index);
                        }
                    }
                    "function_call" => {
                        if let Some(ResponsesSlot::ToolCall {
                            content_index,
                            partial_json,
                        }) = output_slots.get_mut(&output_index)
                        {
                            let idx = *content_index;
                            let final_args = item
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let parsed_args = parse_streaming_json(if final_args.is_empty() {
                                partial_json
                            } else {
                                &final_args
                            });
                            if let Some(ContentBlock::ToolCall { arguments, .. }) =
                                output.content_mut().get_mut(idx)
                            {
                                *arguments = parsed_args;
                            }
                            let tool_call = output.content()[idx].clone();
                            push(AssistantMessageEvent::ToolCallEnd {
                                content_index: idx,
                                tool_call,
                                partial: output.clone(),
                            });
                            output_slots.remove(&output_index);
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                saw_terminal_response_event = true;
                let response = parsed.get("response").cloned().unwrap_or(json!({}));
                // Backfill reasoning signatures from the terminal response.
                if let Some(rest_output) = response.get("output").and_then(|o| o.as_array()) {
                    for item in rest_output {
                        if item.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                            if let Some(encrypted) =
                                item.get("encrypted_content").and_then(|e| e.as_str())
                            {
                                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                                    if let Some(ContentBlock::Thinking {
                                        thinking_signature: Some(sig),
                                        ..
                                    }) = reasoning_signatures_by_id.get_mut(id)
                                    {
                                        if let Ok(mut stored) = serde_json::from_str::<Value>(sig) {
                                            if stored
                                                .get("encrypted_content")
                                                .and_then(|v| v.as_str())
                                                .is_none()
                                            {
                                                stored["encrypted_content"] = json!(encrypted);
                                                // Update the live block too.
                                                *sig = stored.to_string();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Re-read live blocks from the message to apply backfill.
                    let _ = &rest_output;
                }
                if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                    output.set_response_id(id.to_string());
                }
                if let Some(usage) = response.get("usage") {
                    let cached = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let cache_write = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cache_write_tokens"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let mut u = Usage {
                        input: input.saturating_sub(cached + cache_write),
                        output: usage
                            .get("output_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0),
                        cache_read: cached,
                        cache_write,
                        reasoning: usage
                            .get("output_tokens_details")
                            .and_then(|d| d.get("reasoning_tokens"))
                            .and_then(|v| v.as_i64()),
                        total_tokens: usage
                            .get("total_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0),
                        cache_write_1h: None,
                        cost: Default::default(),
                    };
                    let cost = calculate_cost(model, &u);
                    u.cost = cost;
                    output.set_usage(u);
                }
                // Service tier pricing multiplier.
                let service_tier = response
                    .get("service_tier")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| options.service_tier.clone());
                if let Some(tier) = service_tier {
                    apply_service_tier_pricing(output, &tier, &model.id);
                }
                // Status -> stop reason.
                let status = response
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let incomplete_reason = response
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let raw = match &incomplete_reason {
                    Some(reason) => format!("{status}.{reason}"),
                    None => status.to_string(),
                };
                output.set_raw_stop_reason(raw);
                let (stop_reason, error_message) =
                    map_stop_reason(status, incomplete_reason.as_deref());
                output.set_stop_reason(stop_reason);
                if let Some(err) = error_message {
                    set_msg_error_message(output, err);
                }
                if output
                    .content()
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
                    && output.stop_reason() == Some(StopReason::Stop)
                {
                    output.set_stop_reason(StopReason::ToolUse);
                }
            }
            "error" => {
                let code = parsed.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let message = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
                return Err(format!("Error Code {code}: {message}"));
            }
            "response.failed" => {
                let response = parsed.get("response").cloned().unwrap_or(json!({}));
                let status = response
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                output.set_raw_stop_reason(status.to_string());
                let error = response.get("error");
                let details = response.get("incomplete_details");
                let msg = if let Some(error) = error {
                    let code = error
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let message = error
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("no message");
                    format!("{code}: {message}")
                } else if let Some(reason) = details
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    format!("incomplete: {reason}")
                } else {
                    "Unknown error (no error details in response)".to_string()
                };
                return Err(msg);
            }
            _ => {}
        }
    }

    if !saw_terminal_response_event {
        return Err("OpenAI Responses stream ended before a terminal response event".to_string());
    }
    Ok(())
}

fn set_msg_error_message(output: &mut AssistantMessage, message: String) {
    let AssistantMessage::Assistant { error_message, .. } = output;
    *error_message = Some(message);
}

pub fn apply_service_tier_pricing(
    output: &mut AssistantMessage,
    service_tier: &str,
    model_id: &str,
) {
    let multiplier = match service_tier {
        "flex" => 0.5,
        "priority" => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => return,
    };
    if let Some(usage) = output.usage_mut() {
        let c = &mut usage.cost;
        c.input *= multiplier;
        c.output *= multiplier;
        c.cache_read *= multiplier;
        c.cache_write *= multiplier;
        c.total = c.input + c.output + c.cache_read + c.cache_write;
    }
}

fn map_stop_reason(status: &str, incomplete_reason: Option<&str>) -> (StopReason, Option<String>) {
    if status.is_empty() {
        return (StopReason::Stop, None);
    }
    match status {
        "completed" => (StopReason::Stop, None),
        "incomplete" => {
            if incomplete_reason == Some("max_output_tokens") {
                (StopReason::Length, None)
            } else {
                (
                    StopReason::Error,
                    Some(match incomplete_reason {
                        Some(reason) => format!("Response incomplete: {reason}"),
                        None => "Response incomplete without a provider reason".to_string(),
                    }),
                )
            }
        }
        "failed" | "cancelled" => (StopReason::Error, None),
        "in_progress" | "queued" => (StopReason::Stop, None),
        other => panic!("Unhandled stop reason: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, ModelInput};
    use crate::types::*;

    fn model(id: &str) -> Model {
        let mut m = Model::new(id, id, "openai-responses", "openai");
        m.reasoning = true;
        m.input = vec![ModelInput::Text, ModelInput::Image];
        m
    }

    #[test]
    fn text_signature_roundtrip() {
        let sig = encode_text_signature_v1("msg_abc", Some("final_answer"));
        let (id, phase) = parse_text_signature(Some(&sig));
        assert_eq!(id.as_deref(), Some("msg_abc"));
        assert_eq!(phase.as_deref(), Some("final_answer"));
        let (id, phase) = parse_text_signature(None);
        assert_eq!(id, None);
        assert_eq!(phase, None);
    }

    #[test]
    fn tools_converted_with_strict() {
        let tools = vec![json_tool(
            "bash",
            "run",
            &json!({"type":"object","properties":{}}),
        )];
        let out = convert_responses_tools(
            &tools,
            &ConvertResponsesToolsOptions {
                strict: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(out[0]["type"], "function");
        assert_eq!(out[0]["strict"], true);
        assert_eq!(out[0]["parameters"]["additionalProperties"], false);
    }

    #[test]
    fn messages_system_prompt_developer_for_reasoning() {
        let m = model("gpt-5");
        let ctx = Context {
            system_prompt: Some("be good".into()),
            messages: vec![Message::User(UserContent::string("hi", 1))],
            ..Default::default()
        };
        let out = convert_responses_messages(
            &m,
            &ctx,
            &["openai"],
            &ConvertResponsesMessagesOptions::default(),
        );
        assert_eq!(out[0]["role"], "developer");
        assert_eq!(out[0]["content"], "be good");
        assert_eq!(out[1]["role"], "user");
    }

    #[test]
    fn tool_result_converted_flat() {
        let m = model("gpt-5");
        let ctx = Context {
            messages: vec![Message::ToolResult(ToolResultMessage::new(
                "call_1|fc_1",
                "bash",
                vec![ContentBlock::text("out")],
                false,
            ))],
            ..Default::default()
        };
        let out = convert_responses_messages(
            &m,
            &ctx,
            &["openai"],
            &ConvertResponsesMessagesOptions::default(),
        );
        assert_eq!(out[0]["type"], "function_call_output");
        assert_eq!(out[0]["call_id"], "call_1");
        assert_eq!(out[0]["output"], "out");
    }

    #[test]
    fn tool_result_with_image_uses_data_url() {
        let m = model("gpt-5");
        let ctx = Context {
            messages: vec![Message::ToolResult(ToolResultMessage::new(
                "call_1",
                "bash",
                vec![
                    ContentBlock::text("out"),
                    ContentBlock::image("aGk=", "image/png"),
                ],
                false,
            ))],
            ..Default::default()
        };
        let out = convert_responses_messages(
            &m,
            &ctx,
            &["openai"],
            &ConvertResponsesMessagesOptions::default(),
        );
        let arr = out[0]["output"].as_array().unwrap();
        assert_eq!(arr[0]["type"], "input_text");
        assert_eq!(arr[1]["type"], "input_image");
        assert!(arr[1]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    fn sse(data: &str) -> crate::sse::SseEvent {
        crate::sse::SseEvent {
            data: data.to_string(),
            event: None,
            id: None,
        }
    }

    #[test]
    fn processes_text_and_completed() {
        let m = model("gpt-5");
        let mut output = AssistantMessage::new();
        output.set_api_provider_model("openai-responses", "openai", "gpt-5");
        output.set_stop_reason(StopReason::Pending);
        let events = vec![
            sse(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
            sse(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse(r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hello"}"#),
            sse(r#"{"type":"response.output_text.delta","output_index":0,"delta":" world"}"#),
            sse(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello world","annotations":[]}]}}"#,
            ),
            sse(
                r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
            ),
        ];
        let mut pushes: Vec<AssistantMessageEvent> = Vec::new();
        process_responses_stream(
            &events,
            &mut output,
            &mut |e| pushes.push(e),
            &m,
            &ProcessResponsesOptions::default(),
        )
        .unwrap();
        assert_eq!(output.stop_reason(), Some(StopReason::Stop));
        assert_eq!(output.response_id().unwrap(), "resp_1");
        let usage = output.usage().unwrap();
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5);
        let text: String = output
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(pushes
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
        assert!(pushes
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextEnd { .. })));
        assert!(pushes
            .iter()
            .any(|e| !matches!(e, AssistantMessageEvent::Done { .. })));
        // Message phase stop from final_answer item.
    }

    #[test]
    fn processes_tool_call_with_partial_json() {
        let m = model("gpt-5");
        let mut output = AssistantMessage::new();
        output.set_api_provider_model("openai-responses", "openai", "gpt-5");
        output.set_stop_reason(StopReason::Pending);
        let events = vec![
            sse(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"bash","arguments":"","status":"in_progress"}}"#,
            ),
            sse(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\":"}"#,
            ),
            sse(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"ls\""}"#,
            ),
            sse(
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"cmd\":\"ls\"}"}"#,
            ),
            sse(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}","status":"completed"}}"#,
            ),
            sse(r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#),
        ];
        let mut pushes: Vec<AssistantMessageEvent> = Vec::new();
        process_responses_stream(
            &events,
            &mut output,
            &mut |e| pushes.push(e),
            &m,
            &ProcessResponsesOptions::default(),
        )
        .unwrap();
        // Tool call present -> stop reason becomes toolUse on completed.
        assert_eq!(output.stop_reason(), Some(StopReason::ToolUse));
        match &output.content()[0] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_abc|fc_1");
                assert_eq!(name, "bash");
                assert_eq!(arguments["cmd"], "ls");
            }
            b => panic!("expected toolCall: {b:?}"),
        }
    }

    #[test]
    fn incomplete_max_output_is_length() {
        let m = model("gpt-5");
        let mut output = AssistantMessage::new();
        output.set_api_provider_model("openai-responses", "openai", "gpt-5");
        output.set_stop_reason(StopReason::Pending);
        let events = vec![sse(
            r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        )];
        process_responses_stream(
            &events,
            &mut output,
            &mut |_| {},
            &m,
            &ProcessResponsesOptions::default(),
        )
        .unwrap();
        assert_eq!(output.stop_reason(), Some(StopReason::Length));
        assert_eq!(
            output.raw_stop_reason().unwrap(),
            "incomplete.max_output_tokens"
        );
    }

    #[test]
    fn missing_terminal_event_errors() {
        let m = model("gpt-5");
        let mut output = AssistantMessage::new();
        output.set_api_provider_model("openai-responses", "openai", "gpt-5");
        let events = vec![sse(
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
        )];
        let err = process_responses_stream(
            &events,
            &mut output,
            &mut |_| {},
            &m,
            &ProcessResponsesOptions::default(),
        )
        .unwrap_err();
        assert!(err.contains("before a terminal response event"));
    }

    #[test]
    fn reasoning_replays_signature() {
        let m = model("gpt-5");
        let mut output = AssistantMessage::new();
        output.set_api_provider_model("openai-responses", "openai", "gpt-5");
        output.set_stop_reason(StopReason::Pending);
        let reasoning_item = r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking"}],"content":[],"status":"completed"}"#;
        let events = vec![
            sse(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","status":"in_progress"}}"#,
            ),
            sse(r#"{"type":"response.reasoning_text.delta","output_index":0,"delta":"think"}"#),
            sse(&format!(
                r#"{{"type":"response.output_item.done","output_index":0,"item":{reasoning_item}}}"#
            )),
            sse(r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#),
        ];
        process_responses_stream(
            &events,
            &mut output,
            &mut |_| {},
            &m,
            &ProcessResponsesOptions::default(),
        )
        .unwrap();
        match &output.content()[0] {
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                assert_eq!(thinking, "thinking");
                let sig = thinking_signature.as_ref().unwrap();
                let parsed: Value = serde_json::from_str(sig).unwrap();
                assert_eq!(parsed["type"], "reasoning");
                assert_eq!(parsed["id"], "rs_1");
            }
            b => panic!("expected thinking: {b:?}"),
        }
    }
}
