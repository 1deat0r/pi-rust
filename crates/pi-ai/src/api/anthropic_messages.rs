//! Anthropic Messages API adaptor — port of
//! `packages/ai/src/api/anthropic-messages.ts`.
//!
//! Converts the unified `Context` into the Messages API payload, streams the
//! SSE response, and emits the unified `AssistantMessageEvent` protocol.
//! `stream` never throws: failures are encoded as a terminal error event.

use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{calculate_cost, Model, ModelCost};
use crate::sse::SseParser;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, ErrorReason,
    Message, ModelThinkingLevel, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions,
    ThinkingLevel, Tool, ToolChoice, Usage,
};

use super::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use super::transform_messages::transform_messages;

pub const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

const CLAUDE_CODE_VERSION: &str = "2.1.75";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

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
#[derive(Clone, Default)]
pub struct AnthropicOptions {
    pub base: StreamOptions,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub tool_choice: Option<ToolChoice>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    pub effort: Option<String>,
    /// Whether to request interleaved thinking for non-adaptive models.
    /// Upstream defaults this to true.
    pub interleaved_thinking: Option<bool>,
}

#[derive(Debug, Clone)]
struct AllowedFallbackModel {
    provider: String,
    model: String,
    cost: Option<ModelCost>,
}

#[derive(Debug, Clone, Copy)]
struct AnthropicCompat {
    supports_eager_tool_input_streaming: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    force_adaptive_thinking: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    supports_tool_references: bool,
}

fn compat_bool(model: &Model, key: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let Some(rest) = model.id.strip_prefix("claude-").and_then(|rest| {
        rest.strip_prefix("opus-")
            .or_else(|| rest.strip_prefix("sonnet-"))
            .or_else(|| rest.strip_prefix("fable-"))
    }) else {
        return false;
    };
    let mut pieces = rest.split('-');
    let Ok(major) = pieces.next().unwrap_or_default().parse::<u64>() else {
        return false;
    };
    let minor = pieces
        .next()
        .filter(|piece| piece.len() < 8)
        .and_then(|piece| piece.parse::<u64>().ok())
        .unwrap_or(0);
    major > 4 || (major == 4 && minor >= 5)
}

fn anthropic_compat(model: &Model) -> AnthropicCompat {
    AnthropicCompat {
        supports_eager_tool_input_streaming: compat_bool(
            model,
            "supportsEagerToolInputStreaming",
            true,
        ),
        supports_long_cache_retention: compat_bool(model, "supportsLongCacheRetention", true),
        send_session_affinity_headers: compat_bool(model, "sendSessionAffinityHeaders", false),
        supports_cache_control_on_tools: compat_bool(model, "supportsCacheControlOnTools", true),
        supports_temperature: compat_bool(model, "supportsTemperature", true),
        force_adaptive_thinking: compat_bool(model, "forceAdaptiveThinking", false),
        allow_empty_signature: compat_bool(model, "allowEmptySignature", false),
        supports_strict_tools: compat_bool(model, "supportsStrictTools", false),
        supports_tool_references: model
            .compat
            .as_ref()
            .and_then(|compat| compat.get("supportsToolReferences"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| default_supports_tool_references(model)),
    }
}

fn allowed_fallback_models(model: &Model) -> Vec<AllowedFallbackModel> {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("allowedFallbackModels"))
        .and_then(Value::as_array)
        .map(|fallbacks| {
            fallbacks
                .iter()
                .filter_map(|fallback| {
                    let object = fallback.as_object()?;
                    let provider = object.get("provider")?.as_str()?.to_string();
                    let fallback_model = object.get("model")?.as_str()?.to_string();
                    let cost = object
                        .get("cost")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok());
                    Some(AllowedFallbackModel {
                        provider,
                        model: fallback_model,
                        cost,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|canonical| canonical.eq_ignore_ascii_case(name))
        .map(|canonical| (*canonical).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    tools
        .iter()
        .find(|tool| tool.name.eq_ignore_ascii_case(name))
        .map(|tool| tool.name.clone())
        .unwrap_or_else(|| name.to_string())
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
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

/// Converts unified messages to Anthropic `MessageParam`s.
///
/// This compatibility wrapper keeps the original Rust call surface. The
/// request path below uses `convert_messages_with_options` so OAuth tool
/// names, deferred references, and same-model thinking replay are all applied
/// together.
pub fn convert_messages(messages: &[Message], allow_empty_signature: bool) -> Vec<Value> {
    convert_messages_with_options(
        messages,
        false,
        allow_empty_signature,
        &HashSet::new(),
        &|name| name.to_string(),
        None,
    )
}

fn convert_content_blocks(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    if !has_images {
        return json!(content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }

    json!(content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
            ContentBlock::Image {
                data, mime_type, ..
            } => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime_type, "data": data},
            })),
            _ => None,
        })
        .collect::<Vec<_>>())
}

fn convert_tool_result(
    result: &crate::types::ToolResultMessage,
    deferred_tool_names: &HashSet<String>,
    loaded_tool_names: &mut HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (Value, Vec<Value>) {
    let mut references = Vec::new();
    if let Some(added_tool_names) = match result {
        crate::types::ToolResultMessage::ToolResult {
            added_tool_names, ..
        } => added_tool_names.as_ref(),
    } {
        for name in added_tool_names {
            let normalized_name = normalize_tool_name(name);
            if !deferred_tool_names.contains(&normalized_name)
                || loaded_tool_names.contains(&normalized_name)
            {
                continue;
            }
            loaded_tool_names.insert(normalized_name);
            references.push(json!({
                "type": "tool_reference",
                "tool_name": normalize_tool_name(name),
            }));
        }
    }

    let converted_content = convert_content_blocks(result.content());
    let sibling_content = if references.is_empty() {
        Vec::new()
    } else if converted_content.is_string() {
        vec![json!({"type": "text", "text": converted_content})]
    } else {
        converted_content.as_array().cloned().unwrap_or_default()
    };

    let is_error = result.is_error();
    let tool_result = json!({
        "type": "tool_result",
        "tool_use_id": result.tool_call_id(),
        "content": if references.is_empty() { converted_content } else { json!(references) },
        "is_error": is_error,
    });
    (tool_result, sibling_content)
}

fn convert_messages_with_options(
    messages: &[Message],
    is_oauth_token: bool,
    allow_empty_signature: bool,
    deferred_tool_names: &HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
    cache_control: Option<&Value>,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let mut loaded_tool_names = HashSet::new();
    for (index, msg) in messages.iter().enumerate() {
        match msg {
            Message::User(user) => match user.content() {
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
            Message::Assistant(assistant) => {
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
                            let input = if arguments.is_null() {
                                json!({})
                            } else {
                                arguments.clone()
                            };
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": if is_oauth_token { to_claude_code_name(name) } else { name.clone() },
                                "input": input,
                            }));
                        }
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    params.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResult(_result) => {
                let mut tool_results = Vec::new();
                let mut sibling_content = Vec::new();
                let mut cursor = index;
                while let Some(Message::ToolResult(next)) = messages.get(cursor) {
                    let (tool_result, siblings) = convert_tool_result(
                        next,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_tool_name,
                    );
                    tool_results.push(tool_result);
                    sibling_content.extend(siblings);
                    cursor += 1;
                }
                // The outer loop cannot be advanced directly, so duplicate
                // consecutive tool results are filtered below by checking the
                // previous message role.
                if index > 0 && matches!(messages[index - 1], Message::ToolResult(_)) {
                    continue;
                }
                let mut content = tool_results;
                content.extend(sibling_content);
                params.push(json!({"role": "user", "content": content}));
            }
        }
    }

    if let Some(cache_control) = cache_control {
        if let Some(last_message) = params.last_mut() {
            if last_message.get("role").and_then(Value::as_str) == Some("user") {
                if let Some(content) = last_message.get_mut("content") {
                    if let Some(blocks) = content.as_array_mut() {
                        if let Some(last_block) = blocks.last_mut() {
                            let block_type = last_block.get("type").and_then(Value::as_str);
                            if matches!(
                                block_type,
                                Some("text") | Some("image") | Some("tool_result")
                            ) {
                                last_block["cache_control"] = cache_control.clone();
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        *content = json!([{
                            "type": "text",
                            "text": text,
                            "cache_control": cache_control,
                        }]);
                    }
                }
            }
        }
    }

    params
}

/// Converts unified tools to Anthropic `ToolParam`s, including the provider's
/// strict-schema extension when the model advertises it.
pub fn convert_tools(
    tools: &[Tool],
    cache_control: bool,
    supports_strict_tools: bool,
) -> Result<Vec<Value>, String> {
    convert_tools_with_options(
        tools,
        false,
        true,
        supports_strict_tools,
        if cache_control {
            Some(json!({"type": "ephemeral"}))
        } else {
            None
        },
        false,
    )
}

fn convert_tools_with_options(
    tools: &[Tool],
    is_oauth_token: bool,
    supports_eager_tool_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control: Option<Value>,
    defer_loading: bool,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict_tools)?;
            let parameters = get_json_schema_tool_parameters(tool, strict)?;
            let schema = parameters.as_object();
            let mut legacy_input_schema = json!({
                "type": "object",
                "properties": schema
                    .and_then(|schema| schema.get("properties"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                "required": schema
                    .and_then(|schema| schema.get("required"))
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            });
            let input_schema = if strict == Some(true) {
                let mut full = parameters.clone();
                if let (Some(full), Some(legacy)) =
                    (full.as_object_mut(), legacy_input_schema.as_object_mut())
                {
                    for (key, value) in legacy.iter() {
                        full.insert(key.clone(), value.clone());
                    }
                }
                full
            } else {
                legacy_input_schema
            };
            let mut value = json!({
                "name": if is_oauth_token { to_claude_code_name(&tool.name) } else { tool.name.clone() },
                "description": tool.description,
                "input_schema": input_schema,
            });
            if supports_eager_tool_input_streaming {
                value["eager_input_streaming"] = json!(true);
            }
            if strict == Some(true) {
                value["strict"] = json!(true);
            }
            if defer_loading {
                value["defer_loading"] = json!(true);
            }
            if index + 1 == tools.len() {
                if let Some(cache_control) = &cache_control {
                    value["cache_control"] = cache_control.clone();
                }
            }
            Ok(value)
        })
        .collect()
}

fn resolve_cache_retention(
    cache_retention: Option<&String>,
    env: Option<&BTreeMap<String, String>>,
) -> String {
    if let Some(value) = cache_retention {
        return value.clone();
    }
    let configured = env
        .and_then(|values| values.get("PI_CACHE_RETENTION"))
        .cloned()
        .or_else(|| std::env::var("PI_CACHE_RETENTION").ok());
    if configured.as_deref() == Some("long") {
        "long".to_string()
    } else {
        "short".to_string()
    }
}

fn cache_control_for_model(model: &Model, cache_retention: &str) -> Option<Value> {
    if cache_retention == "none" {
        return None;
    }
    let mut cache_control = json!({"type": "ephemeral"});
    if cache_retention == "long" && anthropic_compat(model).supports_long_cache_retention {
        cache_control["ttl"] = json!("1h");
    }
    Some(cache_control)
}

fn split_deferred_tools(
    messages: &[Message],
    tools: &[Tool],
    enabled: bool,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (Vec<Tool>, Vec<Tool>) {
    let mut unique_tools: Vec<(String, Tool)> = Vec::new();
    for tool in tools {
        let normalized = normalize_tool_name(&tool.name);
        if let Some((_, existing)) = unique_tools
            .iter_mut()
            .find(|(name, _)| name == &normalized)
        {
            *existing = tool.clone();
        } else {
            unique_tools.push((normalized, tool.clone()));
        }
    }
    if !enabled {
        return (
            unique_tools.into_iter().map(|(_, tool)| tool).collect(),
            Vec::new(),
        );
    }

    let mut used_names = HashSet::new();
    let mut deferred_names = HashSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for block in assistant.content() {
                    if let ContentBlock::ToolCall { name, .. } = block {
                        used_names.insert(normalize_tool_name(name));
                    }
                }
            }
            Message::ToolResult(result) => {
                if let crate::types::ToolResultMessage::ToolResult {
                    added_tool_names: Some(names),
                    ..
                } = result
                {
                    for name in names {
                        let normalized = normalize_tool_name(name);
                        if !used_names.contains(&normalized) {
                            deferred_names.insert(normalized);
                        }
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = Vec::new();
    for (name, tool) in unique_tools {
        if deferred_names.contains(&name) {
            deferred.push(tool);
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// Assembles a request-body value (port of upstream `buildParams`).
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &AnthropicOptions,
) -> Result<Value, String> {
    build_params_for_request(model, context, false, options)
}

fn build_params_for_request(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: &AnthropicOptions,
) -> Result<Value, String> {
    let compat = anthropic_compat(model);
    let cache_retention = resolve_cache_retention(
        options.base.cache_retention.as_ref(),
        options.base.base.env.as_ref(),
    );
    let cache_control = cache_control_for_model(model, &cache_retention);

    let normalize_name = |name: &str| {
        if is_oauth_token {
            to_claude_code_name(name)
        } else {
            name.to_string()
        }
    };
    let normalize_tool_call_id_for_model =
        |id: &str, _model: &Model, _source: &AssistantMessage| normalize_tool_call_id(id);
    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(&normalize_tool_call_id_for_model),
    );
    let (mut immediate_tools, mut deferred_tools) = split_deferred_tools(
        &transformed_messages,
        &context.tools,
        compat.supports_tool_references,
        &normalize_name,
    );
    // Anthropic requires at least one immediately available tool. If every
    // active definition was marked deferred, load the definitions normally and
    // suppress both defer_loading and tool_reference blocks.
    if immediate_tools.is_empty() && !deferred_tools.is_empty() {
        immediate_tools.append(&mut deferred_tools);
    }
    let deferred_tool_names: HashSet<String> = deferred_tools
        .iter()
        .map(|tool| normalize_name(&tool.name))
        .collect();

    let mut params = json!({
        "model": model.id,
        "messages": convert_messages_with_options(
            &transformed_messages,
            is_oauth_token,
            compat.allow_empty_signature,
            &deferred_tool_names,
            &normalize_name,
            cache_control.as_ref(),
        ),
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });

    let mut system: Vec<Value> = Vec::new();
    if is_oauth_token {
        let mut identity = json!({"type": "text", "text": CLAUDE_CODE_IDENTITY});
        if let Some(cache_control) = &cache_control {
            identity["cache_control"] = cache_control.clone();
        }
        system.push(identity);
    }
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
        if options.thinking_enabled != Some(true) && compat.supports_temperature {
            params["temperature"] = json!(temperature);
        }
    }

    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let mut tools = convert_tools_with_options(
            &immediate_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            if compat.supports_cache_control_on_tools {
                cache_control.clone()
            } else {
                None
            },
            false,
        )?;
        tools.extend(convert_tools_with_options(
            &deferred_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            None,
            true,
        )?);
        params["tools"] = json!(tools);
    }

    if model.reasoning {
        match options.thinking_enabled {
            Some(false) => {
                if model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get(&ModelThinkingLevel::Off))
                    != Some(&None)
                {
                    params["thinking"] = json!({"type": "disabled"});
                }
            }
            _ => {
                let display = options
                    .thinking_display
                    .unwrap_or(AnthropicThinkingDisplay::Summarized);
                if compat.force_adaptive_thinking {
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

    let fallbacks = allowed_fallback_models(model);
    if !fallbacks.is_empty() {
        params["fallbacks"] = json!(fallbacks
            .iter()
            .map(|fallback| json!({"model": fallback.model}))
            .collect::<Vec<_>>());
    }

    Ok(params)
}

fn provider_env_value(options: &AnthropicOptions, name: &str) -> Option<String> {
    options
        .base
        .base
        .env
        .as_ref()
        .and_then(|env| env.get(name).cloned())
        .or_else(|| std::env::var(name).ok())
}

fn has_auth_header(headers: Option<&ProviderHeaders>) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "x-api-key" | "cf-aig-authorization"
            ) && value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        })
    })
}

fn has_model_auth_header(model: &Model) -> bool {
    model.headers.as_ref().is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "x-api-key" | "cf-aig-authorization"
            ) && !value.trim().is_empty()
        })
    })
}

fn resolve_request_auth(
    model: &Model,
    options: &AnthropicOptions,
    explicit_api_key: Option<&str>,
) -> (Option<String>, bool) {
    if let Some(key) = explicit_api_key.filter(|key| !key.is_empty()) {
        return (Some(key.to_string()), false);
    }
    if let Some(key) = options
        .base
        .base
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    {
        return (Some(key.to_string()), false);
    }
    if has_auth_header(options.base.base.headers.as_ref()) || has_model_auth_header(model) {
        return (None, false);
    }
    if model.provider != "anthropic" {
        return (None, false);
    }
    if let Some(token) =
        provider_env_value(options, "ANTHROPIC_AUTH_TOKEN").filter(|token| !token.is_empty())
    {
        return (Some(token), true);
    }
    if let Some(token) =
        provider_env_value(options, "ANTHROPIC_OAUTH_TOKEN").filter(|token| !token.is_empty())
    {
        return (Some(token), false);
    }
    (
        provider_env_value(options, "ANTHROPIC_API_KEY").filter(|key| !key.is_empty()),
        false,
    )
}

fn apply_model_headers(headers: &mut BTreeMap<String, String>, model: &Model) {
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
}

fn apply_header_overrides(
    headers: &mut BTreeMap<String, String>,
    overrides: Option<&ProviderHeaders>,
) {
    if let Some(overrides) = overrides {
        for (name, value) in overrides {
            let name = name.to_ascii_lowercase();
            if let Some(value) = value {
                headers.insert(name, value.clone());
            } else {
                headers.remove(&name);
            }
        }
    }
}

fn build_anthropic_headers(
    model: &Model,
    context: &Context,
    options: &AnthropicOptions,
    api_key: Option<&str>,
    bearer_auth: bool,
) -> BTreeMap<String, String> {
    let compat = anthropic_compat(model);
    let mut beta_features = Vec::new();
    if !context.tools.is_empty() && !compat.supports_eager_tool_input_streaming {
        beta_features.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if options.interleaved_thinking.unwrap_or(true) && !compat.force_adaptive_thinking {
        beta_features.push(INTERLEAVED_THINKING_BETA);
    }
    if !allowed_fallback_models(model).is_empty() {
        beta_features.push(SERVER_SIDE_FALLBACK_BETA);
    }

    let is_oauth = api_key.is_some_and(is_oauth_token);
    let mut headers = BTreeMap::from([
        (
            "user-agent".to_string(),
            super::mistral_conversations::pi_user_agent(),
        ),
        ("accept".to_string(), "application/json".to_string()),
        (
            "anthropic-dangerous-direct-browser-access".to_string(),
            "true".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION_HEADER.to_string(),
        ),
    ]);

    let cache_retention = resolve_cache_retention(
        options.base.cache_retention.as_ref(),
        options.base.base.env.as_ref(),
    );
    if options.base.session_id.is_some()
        && cache_retention != "none"
        && compat.send_session_affinity_headers
    {
        headers.insert(
            "x-session-affinity".to_string(),
            options.base.session_id.clone().unwrap_or_default(),
        );
    }

    if model.provider == "github-copilot" || bearer_auth || is_oauth {
        if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
            headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
        }
    } else if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        headers.insert("x-api-key".to_string(), api_key.to_string());
    }

    if !beta_features.is_empty() {
        headers.insert("anthropic-beta".to_string(), beta_features.join(","));
    }
    if is_oauth {
        let features = if beta_features.is_empty() {
            String::new()
        } else {
            format!(",{}", beta_features.join(","))
        };
        headers.insert(
            "anthropic-beta".to_string(),
            format!("claude-code-20250219,oauth-2025-04-20{features}"),
        );
        headers.insert(
            "user-agent".to_string(),
            format!("claude-cli/{CLAUDE_CODE_VERSION}"),
        );
        headers.insert("x-app".to_string(), "cli".to_string());
    }

    apply_model_headers(&mut headers, model);
    if model.provider == "github-copilot" {
        let has_images = super::github_copilot_headers::has_copilot_vision_input(&context.messages);
        for (name, value) in super::github_copilot_headers::build_copilot_dynamic_headers(
            &context.messages,
            has_images,
        ) {
            headers.insert(name.to_ascii_lowercase(), value);
        }
    }
    apply_header_overrides(&mut headers, options.base.base.headers.as_ref());
    headers
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
    push: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    process_anthropic_events_with_options(model, events, false, &[], push)
}

fn process_anthropic_events_with_options(
    model: &Model,
    events: &[crate::sse::SseEvent],
    is_oauth_token: bool,
    tools: &[Tool],
    mut push: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    let mut output = new_output(model);

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
    let mut usage_model = model.clone();
    let mut saw_message_start = false;
    let mut saw_message_stop = false;

    for event in events {
        let event_type = event.event.as_deref().unwrap_or("");
        if event_type == "error" {
            return Err(event.data.clone());
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(error) => {
                let known = matches!(
                    event_type,
                    "message_start"
                        | "message_delta"
                        | "message_stop"
                        | "content_block_start"
                        | "content_block_delta"
                        | "content_block_stop"
                );
                if known {
                    return Err(format!(
                        "Could not parse Anthropic SSE event {event_type}: {error}; data={}",
                        event.data
                    ));
                }
                continue;
            }
        };
        let t = data
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(event_type);
        let known = matches!(
            t,
            "message_start"
                | "message_delta"
                | "message_stop"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "error"
        );
        if !known {
            continue;
        }
        match t {
            "message_start" => {
                saw_message_start = true;
                output = new_output(model);
                live.clear();
                let message = &data["message"];
                if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                    output.set_response_id(id.to_string());
                }
                usage_model = model.clone();
                if let Some(response_model) = message.get("model").and_then(|v| v.as_str()) {
                    if !response_model.is_empty() && response_model != model.id {
                        output.set_response_model(response_model.to_string());
                        if let Some(fallback) =
                            allowed_fallback_models(model).into_iter().find(|fallback| {
                                fallback.provider == model.provider
                                    && fallback.model == response_model
                            })
                        {
                            if let Some(cost) = fallback.cost {
                                usage_model.id = response_model.to_string();
                                usage_model.cost = cost;
                            }
                        }
                    }
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
                usage.cost = calculate_cost(&usage_model, &usage);
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
                        output.content_mut().push(ContentBlock::text(&text));
                        push(AssistantMessageEvent::TextStart {
                            content_index,
                            partial: output.clone(),
                        });
                    }
                    "thinking" => {
                        let thinking = block
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
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
                            thinking,
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
                        let input = block
                            .get("input")
                            .cloned()
                            .filter(|input| !input.is_null())
                            .unwrap_or_else(|| json!({}));
                        let name = if is_oauth_token {
                            from_claude_code_name(&name, tools)
                        } else {
                            name
                        };
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
                            .push(ContentBlock::tool_call(id, name, input));
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
                        .get("cache_creation")
                        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                        .and_then(|v| v.as_i64())
                    {
                        current.cache_write_1h = Some(v);
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
                    current.cost = calculate_cost(&usage_model, &current);
                    output.set_usage(current);
                }
            }
            "message_stop" => {
                saw_message_stop = true;
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

    if saw_message_start && !saw_message_stop {
        return Err("Anthropic stream ended before message_stop".to_string());
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
    let explicit_api_key = api_key.map(str::to_string);
    let base_url = base_url.to_string();

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let (api_key, bearer_auth) =
            resolve_request_auth(&model, &options, explicit_api_key.as_deref());
        if api_key.is_none()
            && !has_auth_header(options.base.base.headers.as_ref())
            && !has_model_auth_header(&model)
        {
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            set_error_message(
                &mut message,
                format!("No API key for provider: {}", model.provider),
            );
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        let is_oauth = api_key.as_deref().is_some_and(is_oauth_token);
        let params = match build_params_for_request(&model, &context, is_oauth, &options) {
            Ok(params) => params,
            Err(error) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, error);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let headers =
            build_anthropic_headers(&model, &context, &options, api_key.as_deref(), bearer_auth);
        let mut request = client
            .post(format!("{}/v1/messages", base_url.trim_end_matches('/')))
            .json(&params);
        for (name, value) in headers {
            request = request.header(name, value);
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
        let response_headers: BTreeMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();
        // Invoke on_response before consuming the body.
        let provider_response = crate::types::ProviderResponse {
            status: status.as_u16(),
            headers: response_headers,
        };
        if let Some(on_response) = &options.base.on_response {
            on_response(&provider_response, &model);
        }
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

        pusher.push(AssistantMessageEvent::Start {
            partial: new_output(&model),
        });
        let assembled = process_anthropic_events_with_options(
            &model,
            &events,
            is_oauth,
            &context.tools,
            |event| pusher.push(event),
        );
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
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|levels| levels.get(&ModelThinkingLevel::from(level)))
        .cloned()
        .flatten()
        .unwrap_or_else(|| match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low".to_string(),
            ThinkingLevel::Medium => "medium".to_string(),
            ThinkingLevel::High => "high".to_string(),
            ThinkingLevel::Xhigh | ThinkingLevel::Max => "high".to_string(),
        })
}

fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> (u64, u64) {
    let level = match reasoning_level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        other => other,
    };
    let thinking_budget = match custom_budgets {
        Some(budgets) => match level {
            ThinkingLevel::Minimal => budgets.minimal,
            ThinkingLevel::Low => budgets.low,
            ThinkingLevel::Medium => budgets.medium,
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => budgets.high,
        },
        None => None,
    }
    .unwrap_or(match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16384,
    });
    let max_tokens = base_max_tokens
        .map(|base| base.saturating_add(thinking_budget).min(model_max_tokens))
        .unwrap_or(model_max_tokens);
    let thinking_budget = if max_tokens <= thinking_budget {
        thinking_budget.min(max_tokens.saturating_sub(1024))
    } else {
        thinking_budget
    };
    (max_tokens, thinking_budget)
}

fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let available = model
        .context_window
        .saturating_sub(crate::utils::estimate_context_tokens(context).tokens)
        .saturating_sub(4096);
    max_tokens.min(available.max(1))
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let base = AnthropicOptions {
        base: options.base.clone(),
        max_tokens: options.base.max_tokens,
        temperature: options.base.temperature,
        tool_choice: options.tool_choice,
        ..Default::default()
    };
    let Some(reasoning) = options.reasoning else {
        return stream(
            model,
            context,
            client,
            base_url,
            api_key,
            &AnthropicOptions {
                thinking_enabled: Some(false),
                ..base
            },
        );
    };

    let reasoning = crate::model::clamp_thinking_level(model, reasoning.into());
    let level = match reasoning {
        ModelThinkingLevel::Off => {
            return stream(
                model,
                context,
                client,
                base_url,
                api_key,
                &AnthropicOptions {
                    thinking_enabled: Some(false),
                    ..base
                },
            );
        }
        ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        ModelThinkingLevel::Low => ThinkingLevel::Low,
        ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        ModelThinkingLevel::High => ThinkingLevel::High,
        ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        ModelThinkingLevel::Max => ThinkingLevel::Max,
    };
    if anthropic_compat(model).force_adaptive_thinking {
        return stream(
            model,
            context,
            client,
            base_url,
            api_key,
            &AnthropicOptions {
                thinking_enabled: Some(true),
                effort: Some(map_thinking_level_to_effort(model, level)),
                ..base
            },
        );
    }

    let (max_tokens, thinking_budget) = adjust_max_tokens_for_thinking(
        base.max_tokens,
        model.max_tokens,
        level,
        options.thinking_budgets.as_ref(),
    );
    let max_tokens = clamp_max_tokens_to_context(model, context, max_tokens);
    stream(
        model,
        context,
        client,
        base_url,
        api_key,
        &AnthropicOptions {
            max_tokens: Some(max_tokens),
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(thinking_budget.min(max_tokens.saturating_sub(1024))),
            ..base
        },
    )
}

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
    let AssistantMessage::Assistant { error_message, .. } = message;
    *error_message = Some(text);
}
