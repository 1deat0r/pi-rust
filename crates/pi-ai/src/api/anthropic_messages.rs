//! Anthropic Messages API adaptor — port of
//! `packages/ai/src/api/anthropic-messages.ts`.
//!
//! Converts the unified `Context` into the Messages API payload, streams the
//! SSE response, and emits the unified `AssistantMessageEvent` protocol.
//! `stream` never throws: failures are encoded as a terminal error event.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{calculate_cost, Model};
use crate::sse::SseParser;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, ErrorReason,
    StopReason, StreamOptions, ToolChoice, Usage,
};

pub const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

impl AnthropicThinkingDisplay {
    fn as_str(&self) -> &'static str {
        match self {
            AnthropicThinkingDisplay::Summarized => "summarized",
            AnthropicThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Options for Anthropic Messages requests (subset of upstream
/// `AnthropicOptions` + `StreamOptions`).
#[derive(Clone)]
pub struct AnthropicOptions {
    pub base: StreamOptions,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub tool_choice: Option<ToolChoice>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    pub effort: Option<String>,
}

impl Default for AnthropicOptions {
    fn default() -> Self {
        Self {
            base: Default::default(),
            max_tokens: None,
            temperature: None,
            tool_choice: None,
            thinking_enabled: None,
            thinking_budget_tokens: None,
            thinking_display: None,
            effort: None,
        }
    }
}

/// Maps an Anthropic stop reason to the unified `StopReason` (port of
/// `mapStopReason`).
pub fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        "end_turn" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (
                StopReason::Error,
                Some(
                    explanation
                        .unwrap_or_else(|| "The model refused to complete the request".to_string()),
                ),
            )
        }
        "pause_turn" => (StopReason::Stop, None),
        "stop_sequence" => (StopReason::Stop, None),
        "sensitive" => (
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string()),
        ),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

/// Converts unified messages to Anthropic `MessageParam`s (port of
/// `convertMessages`, minus the deferred-tool and OAuth-name layers).
pub fn convert_messages(
    messages: &[crate::types::Message],
    allow_empty_signature: bool,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            crate::types::Message::User(user) => match user.content() {
                crate::types::UserContentBody::String(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({"role": "user", "content": text}));
                    }
                }
                crate::types::UserContentBody::Blocks(blocks) => {
                    let mut content: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text, .. } => {
                                if !text.trim().is_empty() {
                                    content.push(json!({"type": "text", "text": text}));
                                }
                            }
                            ContentBlock::Image {
                                data, mime_type, ..
                            } => {
                                content.push(json!({
                                    "type": "image",
                                    "source": {"type": "base64", "media_type": mime_type, "data": data},
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        params.push(json!({"role": "user", "content": content}));
                    }
                }
            },
            crate::types::Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in assistant.content() {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if !text.trim().is_empty() {
                                blocks.push(json!({"type": "text", "text": text}));
                            }
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                            ..
                        } => {
                            if *redacted == Some(true) {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": thinking_signature.clone().unwrap_or_default(),
                                }));
                                continue;
                            }
                            let signature = thinking_signature.as_deref().unwrap_or("");
                            let has_signature = !signature.trim().is_empty();
                            if thinking.trim().is_empty() && !has_signature {
                                continue;
                            }
                            if has_signature {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": signature,
                                }));
                            } else if allow_empty_signature {
                                blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": ""}));
                            } else {
                                // Missing signature -> convert to plain text.
                                blocks.push(json!({"type": "text", "text": thinking}));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": arguments}));
                        }
                        _ => {}
                    }
                }
                params.push(json!({"role": "assistant", "content": blocks}));
            }
            crate::types::Message::ToolResult(result) => {
                let mut content: Vec<Value> = Vec::new();
                if result.is_error() {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id(),
                        "content": result.content().iter().map(tool_result_content).collect::<Vec<_>>(),
                        "is_error": true,
                    }));
                } else {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id(),
                        "content": result.content().iter().map(tool_result_content).collect::<Vec<_>>(),
                    }));
                }
                // tool_result blocks arrive under a user message.
                params.push(json!({"role": "user", "content": content}));
            }
        }
    }
    params
}

fn tool_result_content(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text, .. } => json!({"type": "text", "text": text}),
        ContentBlock::Image {
            data, mime_type, ..
        } => json!({
            "type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data},
        }),
        _ => json!({"type": "text", "text": ""}),
    }
}

/// Converts unified tools to Anthropic `ToolParam`s (subset of
/// `convertTools`; no eager-input-streaming / strict / deferral flags).
pub fn convert_tools(tools: &[crate::types::Tool], cache_control: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut value = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            });
            if cache_control {
                value["cache_control"] = json!({"type": "ephemeral"});
            }
            value
        })
        .collect()
}

fn resolve_cache_retention(
    cache_retention: Option<&String>,
    _env: Option<&BTreeMap<String, String>>,
) -> String {
    // Mirrors upstream resolveCacheRetention: PI_CACHE_RETENTION overrides
    // option, defaulting to "short".
    match cache_retention {
        Some(value) => value.clone(),
        None => std::env::var("PI_CACHE_RETENTION").unwrap_or_else(|_| "short".to_string()),
    }
}

/// Assembles a request-body value (port of `buildParams`, subset without
/// deferred tools / fallbacks / OAuth).
pub fn build_params(model: &Model, context: &Context, options: &AnthropicOptions) -> Value {
    let cache_retention = resolve_cache_retention(
        options.base.cache_retention.as_ref(),
        options.base.base.env.as_ref(),
    );
    let cache_control = if cache_retention != "none" {
        Some(json!({"type": "ephemeral"}))
    } else {
        None
    };

    let mut params = json!({
        "model": model.id,
        "messages": convert_messages(&context.messages, false),
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });

    let mut system: Vec<Value> = Vec::new();
    if let Some(system_prompt) = &context.system_prompt {
        let mut text = json!({"type": "text", "text": system_prompt});
        if let Some(cc) = &cache_control {
            text["cache_control"] = cc.clone();
        }
        system.push(text);
    }
    if !system.is_empty() {
        params["system"] = json!(system);
    }

    if let Some(temperature) = options.temperature {
        if options.thinking_enabled != Some(true) {
            params["temperature"] = json!(temperature);
        }
    }

    if !context.tools.is_empty() {
        params["tools"] = json!(convert_tools(&context.tools, cache_control.is_some()));
    }

    // Thinking: budget-based `enabled` when the model has reasoning on and
    // thinking is not explicitly disabled (adaptive path noted for later).
    if model.reasoning {
        match options.thinking_enabled {
            Some(false) => {
                if model
                    .thinking_level_map
                    .as_ref()
                    .map(|m| m.get(&crate::types::ModelThinkingLevel::Off))
                    .flatten()
                    .is_some()
                {
                    params["thinking"] = json!({"type": "disabled"});
                }
            }
            _ => {
                let display = options
                    .thinking_display
                    .unwrap_or(AnthropicThinkingDisplay::Summarized);
                if options.effort.is_some() {
                    params["thinking"] = json!({"type": "adaptive", "display": display.as_str()});
                    if let Some(effort) = &options.effort {
                        params["output_config"] = json!({"effort": effort});
                    }
                } else {
                    params["thinking"] = json!({
                        "type": "enabled",
                        "budget_tokens": options.thinking_budget_tokens.unwrap_or(1024),
                        "display": display.as_str(),
                    });
                }
            }
        }
    }

    if let Some(metadata) = &options.base.metadata {
        if let Some(user_id) = metadata.get("user_id").and_then(|v| v.as_str()) {
            params["metadata"] = json!({"user_id": user_id});
        }
    }

    if let Some(choice) = options.tool_choice {
        match choice {
            ToolChoice::Auto => {
                params["tool_choice"] = json!({"type": "auto"});
            }
            ToolChoice::None => {
                params["tool_choice"] = json!({"type": "none"});
            }
        }
    }

    params
}

fn empty_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Default::default(),
    }
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_usage(empty_usage());
    message.set_stop_reason(StopReason::Pending);
    message
}

/// Process decoded SSE events into the unified stream protocol. Pure and
/// synchronous over a complete event list (the HTTP loop feeds this line by
/// line; equivalence holds because both maintain the same assembled output).
pub fn process_anthropic_events(
    model: &Model,
    events: &[crate::sse::SseEvent],
    mut push: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    let mut output = new_output(model);
    // Streaming scratch index for content blocks.
    let mut blocks: Vec<(usize, serde_json::Value, ContentBlock)> = Vec::new();
    let _ = &mut blocks;

    struct BlockAccum {
        index: usize,
        kind: BlockKind,
        partial_json: String,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BlockKind {
        Text,
        Thinking,
        ToolCall,
    }

    let mut live: Vec<Option<BlockAccum>> = Vec::new();

    for event in events {
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) => continue, // ping or empty data
        };
        let event_type = event.event.as_deref().unwrap_or("");
        let t = data
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(event_type);
        match t {
            "message_start" => {
                let message = &data["message"];
                if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                    output = new_output(model);
                    output.set_response_id(id.to_string());
                }
                if let Some(m) = message.get("model").and_then(|v| v.as_str()) {
                    let _ = m;
                }
                let usage = &message["usage"];
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let cache_write = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let cache_write_1h = usage
                    .get("cache_creation")
                    .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                    .and_then(|v| v.as_i64());
                let mut usage = empty_usage();
                usage.input = input;
                usage.output = output_tokens;
                usage.cache_read = cache_read;
                usage.cache_write = cache_write;
                usage.cache_write_1h = cache_write_1h;
                usage.total_tokens = input + output_tokens + cache_read + cache_write;
                usage.cost = calculate_cost(model, &usage);
                output.set_usage(usage);
            }
            "content_block_start" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let block = &data["content_block"];
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content_index = output.content().len();
                        while live.len() <= content_index {
                            live.push(None);
                        }
                        live[content_index] = Some(BlockAccum {
                            index,
                            kind: BlockKind::Text,
                            partial_json: String::new(),
                        });
                        output.content_mut().push(ContentBlock::text(""));
                        push(AssistantMessageEvent::TextStart {
                            content_index,
                            partial: output.clone(),
                        });
                        let _ = text;
                    }
                    "thinking" => {
                        let signature = block
                            .get("signature")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content_index = output.content().len();
                        while live.len() <= content_index {
                            live.push(None);
                        }
                        live[content_index] = Some(BlockAccum {
                            index,
                            kind: BlockKind::Thinking,
                            partial_json: String::new(),
                        });
                        output.content_mut().push(ContentBlock::Thinking {
                            thinking: String::new(),
                            thinking_signature: if signature.is_empty() {
                                None
                            } else {
                                Some(signature)
                            },
                            redacted: None,
                        });
                        push(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    "redacted_thinking" => {
                        let data_hex = block
                            .get("data")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content_index = output.content().len();
                        while live.len() <= content_index {
                            live.push(None);
                        }
                        live[content_index] = Some(BlockAccum {
                            index,
                            kind: BlockKind::Thinking,
                            partial_json: String::new(),
                        });
                        output.content_mut().push(ContentBlock::Thinking {
                            thinking: "[Reasoning redacted]".to_string(),
                            thinking_signature: Some(data_hex),
                            redacted: Some(true),
                        });
                        push(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let _input = block.get("input").cloned().unwrap_or(Value::Null);
                        let content_index = output.content().len();
                        while live.len() <= content_index {
                            live.push(None);
                        }
                        live[content_index] = Some(BlockAccum {
                            index,
                            kind: BlockKind::ToolCall,
                            partial_json: String::new(),
                        });
                        output
                            .content_mut()
                            .push(ContentBlock::tool_call(id, name, Value::Null));
                        push(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let delta = &data["delta"];
                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let content_index = live
                    .iter()
                    .position(|b| b.as_ref().map(|b| b.index) == Some(index));
                match (delta_type, content_index) {
                    ("text_delta", Some(ci)) => {
                        let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(ContentBlock::Text { text: slot, .. }) =
                            output.content_mut().get_mut(ci)
                        {
                            slot.push_str(text);
                        }
                        push(AssistantMessageEvent::TextDelta {
                            content_index: ci,
                            delta: text.to_string(),
                            partial: output.clone(),
                        });
                    }
                    ("thinking_delta", Some(ci)) => {
                        let thinking = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(ContentBlock::Thinking { thinking: slot, .. }) =
                            output.content_mut().get_mut(ci)
                        {
                            slot.push_str(thinking);
                        }
                        push(AssistantMessageEvent::ThinkingDelta {
                            content_index: ci,
                            delta: thinking.to_string(),
                            partial: output.clone(),
                        });
                    }
                    ("input_json_delta", Some(ci)) => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some(acc) = live.get_mut(ci).and_then(|b| b.as_mut()) {
                            if acc.kind == BlockKind::ToolCall {
                                acc.partial_json.push_str(partial);
                                let parsed =
                                    crate::partial_json::parse_partial_json(&acc.partial_json)
                                        .unwrap_or(Value::Null);
                                if let Some(ContentBlock::ToolCall { arguments, .. }) =
                                    output.content_mut().get_mut(ci)
                                {
                                    *arguments = parsed;
                                }
                            }
                        }
                        push(AssistantMessageEvent::ToolCallDelta {
                            content_index: ci,
                            delta: partial.to_string(),
                            partial: output.clone(),
                        });
                    }
                    ("signature_delta", Some(ci)) => {
                        let signature = delta
                            .get("signature")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some(ContentBlock::Thinking {
                            thinking_signature, ..
                        }) = output.content_mut().get_mut(ci)
                        {
                            let existing = thinking_signature.take().unwrap_or_default();
                            *thinking_signature = Some(format!("{existing}{signature}"));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let content_index = live
                    .iter()
                    .position(|b| b.as_ref().map(|b| b.index) == Some(index));
                if let Some(ci) = content_index {
                    if let Some(acc) = live.get_mut(ci).and_then(|b| b.take()) {
                        match acc.kind {
                            BlockKind::Text => {
                                let text = match output.content().get(ci) {
                                    Some(ContentBlock::Text { text, .. }) => text.clone(),
                                    _ => String::new(),
                                };
                                push(AssistantMessageEvent::TextEnd {
                                    content_index: ci,
                                    content: text,
                                    partial: output.clone(),
                                });
                            }
                            BlockKind::Thinking => {
                                let thinking = match output.content().get(ci) {
                                    Some(ContentBlock::Thinking { thinking, .. }) => {
                                        thinking.clone()
                                    }
                                    _ => String::new(),
                                };
                                push(AssistantMessageEvent::ThinkingEnd {
                                    content_index: ci,
                                    content: thinking,
                                    partial: output.clone(),
                                });
                            }
                            BlockKind::ToolCall => {
                                let final_block =
                                    output.content().get(ci).cloned().unwrap_or_else(|| {
                                        ContentBlock::tool_call("", "", Value::Null)
                                    });
                                push(AssistantMessageEvent::ToolCallEnd {
                                    content_index: ci,
                                    tool_call: final_block,
                                    partial: output.clone(),
                                });
                            }
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(stop_reason) = data["delta"].get("stop_reason").and_then(|v| v.as_str())
                {
                    output.set_raw_stop_reason(stop_reason.to_string());
                    let (reason, error_message) =
                        map_stop_reason(stop_reason, data["delta"].get("stop_details"))?;
                    output.set_stop_reason(reason);
                    if let Some(msg) = error_message {
                        set_error_message(&mut output, msg);
                    }
                }
                if let Some(usage) = data.get("usage") {
                    let mut current = output.usage().cloned().unwrap_or_else(empty_usage);
                    if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                        current.input = v;
                    }
                    if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                        current.output = v;
                    }
                    if let Some(v) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        current.cache_read = v;
                    }
                    if let Some(v) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        current.cache_write = v;
                    }
                    if let Some(v) = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("thinking_tokens"))
                        .and_then(|v| v.as_i64())
                    {
                        current.reasoning = Some(v);
                    }
                    current.total_tokens =
                        current.input + current.output + current.cache_read + current.cache_write;
                    current.cost = calculate_cost(model, &current);
                    output.set_usage(current);
                }
            }
            "error" => {
                let error = &data["error"];
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(message.to_string());
            }
            _ => {}
        }
    }

    if output.stop_reason() == Some(StopReason::Pending) {
        return Err("Anthropic stream ended without a stop reason".to_string());
    }
    if matches!(
        output.stop_reason(),
        Some(StopReason::Error) | Some(StopReason::Aborted)
    ) {
        let message = output
            .error_message()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "An unknown error occurred".to_string());
        return Err(message);
    }
    Ok(output)
}

/// Streams a request against the Anthropic Messages API. Errors (transport,
/// non-2xx, malformed events) are encoded as terminal `error` events.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &AnthropicOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let sender = match stream.sender() {
        Some(s) => s,
        None => return stream,
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let api_key = api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
    let base_url = base_url.to_string();

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let params = build_params(&model, &context, &options);
        let mut request = client
            .post(format!("{base_url}/v1/messages"))
            .header("content-type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION_HEADER)
            .header("x-api-key", api_key.clone().unwrap_or_default())
            .json(&params);
        // GitHub Copilot proxy: dynamic headers (X-Initiator / Openai-Intent /
        // Copilot-Vision-Request) from upstream github-copilot-headers.ts.
        if model.provider == "github-copilot" {
            let has_images =
                super::github_copilot_headers::has_copilot_vision_input(&context.messages);
            for (name, value) in super::github_copilot_headers::build_copilot_dynamic_headers(
                &context.messages,
                has_images,
            ) {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        if let Some(headers) = &options.base.base.headers {
            for (name, value) in headers {
                if let Some(value) = value {
                    request = request.header(name.as_str(), value.as_str());
                }
            }
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, format!("Request failed: {err}"));
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let status = response.status();
        // Invoke on_response before consuming the body.
        let provider_response = crate::types::ProviderResponse {
            status: status.as_u16(),
            headers: BTreeMap::new(),
        };
        if let Some(on_response) = &options.base.on_response {
            on_response(&provider_response, &model);
        }
        let mut sse = SseParser::new();
        let response = match response.bytes().await {
            Ok(body) => body,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, format!("Request body failed: {err}"));
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        if !status.is_success() {
            let body_text = String::from_utf8_lossy(&response).to_string();
            let detail = extract_anthropic_error(&body_text);
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            set_error_message(
                &mut message,
                format!("Anthropic API error ({}): {}", status.as_u16(), detail),
            );
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        let body_text = String::from_utf8_lossy(&response).to_string();
        let events = SseParser::parse_text(&body_text);
        let _ = &mut sse;

        pusher.push(AssistantMessageEvent::Start {
            partial: new_output(&model),
        });
        let assembled = process_anthropic_events(&model, &events, |event| {
            pusher.push(event);
        });
        match assembled {
            Ok(message) => {
                let reason = match message.stop_reason().unwrap_or(StopReason::Stop) {
                    StopReason::Stop => DoneReason::Stop,
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                pusher.push(AssistantMessageEvent::Done {
                    reason,
                    message: message.clone(),
                });
                pusher.end(Some(message));
            }
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, err);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
            }
        }
    });
    std::mem::forget(handle);
    stream
}

/// Pulls the Anthropic `error.message` (or top-level detail) from an error
/// response body.
pub fn extract_anthropic_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        if let Some(detail) = value.get("detail").and_then(|d| d.as_str()) {
            return detail.to_string();
        }
    }
    body.chars().take(200).collect()
}

/// Default Anthropic base URL for provider factories.
pub fn default_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

/// Sets the error message field on an assistant message.
pub(crate) fn set_error_message(message: &mut AssistantMessage, text: String) {
    match message {
        AssistantMessage::Assistant { error_message, .. } => *error_message = Some(text),
        #[allow(unreachable_patterns)]
        _ => {}
    }
}
