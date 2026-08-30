//! Google Generative AI adaptor — port of
//! `packages/ai/src/api/google-generative-ai.ts` over the public REST
//! `:streamGenerateContent?alt=sse` endpoint (the @google/genai SDK wraps
//! this same HTTP surface).
//!
//! Converts the unified context into Gemini `GenerateContentRequest`, streams
//! SSE chunks of `GenerateContentResponse`, and emits the unified
//! `AssistantMessageEvent` protocol. `stream` never throws: failures are
//! encoded as a terminal error event.

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{calculate_cost, clamp_thinking_level, Model};
use crate::sse::SseParser;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, ErrorReason,
    ModelThinkingLevel, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions,
    ToolChoice, Usage,
};

use super::google_shared::*;
use super::openai_completions::{
    abortable, apply_payload_hook, error_reason, immediate_error_stream, signal_aborted,
    terminal_error_message,
};

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Options for Google requests (subset of upstream `GoogleOptions`).
#[derive(Clone)]
pub struct GoogleOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<String>,
    pub thinking: Option<GoogleThinking>,
}

#[derive(Clone)]
pub struct GoogleThinking {
    pub enabled: bool,
    pub budget_tokens: Option<i64>, // -1 for dynamic, 0 to disable
    pub level: Option<GoogleApiThinkingLevel>,
}

impl GoogleOptions {
    pub fn from_stream_options(base: StreamOptions) -> Self {
        Self {
            base,
            tool_choice: None,
            thinking: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn is_gemma4_model(id: &str) -> bool {
    static GEMMA4: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"(?i)gemma-?4").unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    GEMMA4.is_match(id)
}

fn is_gemini3_pro_model(id: &str) -> bool {
    static GEMINI3_PRO: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"(?i)gemini-3(?:\.\d+)?-pro")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    GEMINI3_PRO.is_match(id)
}

fn is_gemini3_flash_model(id: &str) -> bool {
    static GEMINI3_FLASH: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"gemini-3(?:\.\d+)?-flash")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    let id = id.to_lowercase();
    GEMINI3_FLASH.is_match(&id) || id == "gemini-flash-latest" || id == "gemini-flash-lite-latest"
}

/// `thinkingConfig` when thinking is disabled for a model (upstream
/// `getDisabledThinkingConfig`).
pub fn disabled_thinking_config(model_id: &str) -> Value {
    if is_gemini3_pro_model(model_id) {
        return json!({ "thinkingLevel": "LOW" });
    }
    if is_gemini3_flash_model(model_id) {
        return json!({ "thinkingLevel": "MINIMAL" });
    }
    if is_gemma4_model(model_id) {
        return json!({ "thinkingLevel": "MINIMAL" });
    }
    json!({ "thinkingBudget": 0 })
}

/// Map a resolved level to a Google API ThinkingLevel value (upstream
/// `getThinkingLevel`).
pub fn google_thinking_level(level: ResolvedGoogleThinkingLevel, model_id: &str) -> &'static str {
    if is_gemini3_pro_model(model_id) {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal | ResolvedGoogleThinkingLevel::Low => "LOW",
            ResolvedGoogleThinkingLevel::Medium | ResolvedGoogleThinkingLevel::High => "HIGH",
        };
    }
    if is_gemma4_model(model_id) {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal | ResolvedGoogleThinkingLevel::Low => "MINIMAL",
            ResolvedGoogleThinkingLevel::Medium | ResolvedGoogleThinkingLevel::High => "HIGH",
        };
    }
    match level {
        ResolvedGoogleThinkingLevel::Minimal => "MINIMAL",
        ResolvedGoogleThinkingLevel::Low => "LOW",
        ResolvedGoogleThinkingLevel::Medium => "MEDIUM",
        ResolvedGoogleThinkingLevel::High => "HIGH",
    }
}

/// Token budgets for Gemini 2.x thinking levels (upstream `getGoogleBudget`).
pub fn google_budget(
    model_id: &str,
    level: ResolvedGoogleThinkingLevel,
    custom: Option<&crate::types::ThinkingBudgets>,
) -> i64 {
    if let Some(budgets) = custom {
        let value = match level {
            ResolvedGoogleThinkingLevel::Minimal => budgets.minimal,
            ResolvedGoogleThinkingLevel::Low => budgets.low,
            ResolvedGoogleThinkingLevel::Medium => budgets.medium,
            ResolvedGoogleThinkingLevel::High => budgets.high,
        };
        if let Some(v) = value {
            return v as i64;
        }
    }
    if model_id.contains("2.5-pro") {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal => 128,
            ResolvedGoogleThinkingLevel::Low => 2048,
            ResolvedGoogleThinkingLevel::Medium => 8192,
            ResolvedGoogleThinkingLevel::High => 32768,
        };
    }
    if model_id.contains("2.5-flash-lite") {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal => 512,
            ResolvedGoogleThinkingLevel::Low => 2048,
            ResolvedGoogleThinkingLevel::Medium => 8192,
            ResolvedGoogleThinkingLevel::High => 24576,
        };
    }
    if model_id.contains("2.5-flash") {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal => 128,
            ResolvedGoogleThinkingLevel::Low => 2048,
            ResolvedGoogleThinkingLevel::Medium => 8192,
            ResolvedGoogleThinkingLevel::High => 24576,
        };
    }
    -1
}

/// Assemble the GenerateContentRequest body (port of `buildParams`).
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
) -> Result<Value, String> {
    let contents = convert_messages(model, context);

    let mut generation_config = json!({});
    if let Some(temperature) = options.base.temperature {
        generation_config["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.base.max_tokens {
        generation_config["maxOutputTokens"] = json!(max_tokens);
    }

    let supports_strict = supports_google_strict_tool_sampling(&model.id);
    let function_calling_mode = if !context.tools.is_empty() {
        resolve_google_function_calling_mode(
            &context.tools,
            options.tool_choice.as_deref(),
            supports_strict,
        )?
    } else {
        None
    };

    let mut config = json!({});
    if generation_config
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false)
    {
        config["generationConfig"] = generation_config;
    }
    if let Some(system_prompt) = &context.system_prompt {
        if !system_prompt.is_empty() {
            config["systemInstruction"] = json!({ "parts": [{ "text": system_prompt }] });
        }
    }
    if !context.tools.is_empty() {
        if let Some(tools) = convert_tools(&context.tools, false, supports_strict)? {
            config["tools"] = tools;
        }
    }
    if let Some(mode) = function_calling_mode {
        config["toolConfig"] = json!({ "functionCallingConfig": { "mode": mode } });
    }

    if let Some(thinking) = &options.thinking {
        if thinking.enabled && model.reasoning {
            let mut thinking_config = json!({ "includeThoughts": true });
            if let Some(level) = thinking.level {
                thinking_config["thinkingLevel"] = json!(level);
            } else if let Some(budget) = thinking.budget_tokens {
                thinking_config["thinkingBudget"] = json!(budget);
            }
            config["thinkingConfig"] = thinking_config;
        } else if model.reasoning && !thinking.enabled {
            config["thinkingConfig"] = disabled_thinking_config(&model.id);
        }
    }

    // Flatten config into the top-level REST GenerateContentRequest shape
    // (the @google/genai SDK does this serialization internally from its
    // GenerateContentParameters{model, contents, config}).
    let mut body = json!({
        "contents": contents,
    });
    if let Some(c) = config.as_object() {
        for (k, v) in c {
            body[k] = v.clone();
        }
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Streaming event assembly
// ---------------------------------------------------------------------------

/// Tool-call id generation shared by the stream loop (module-level counter
/// mirroring upstream `toolCallCounter`).
static TOOL_CALL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_tool_call_counter() -> u64 {
    TOOL_CALL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Event-loop state carried across SSE chunks.
struct GoogleStreamState {
    output: AssistantMessage,
    block_kind: Option<BlockKind>,
    blocks: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
}

/// Build the partial assistant snapshot carried by every streamed event.
///
/// The upstream adapter mutates `output.content` as each block is opened and
/// extended, so observers see the live transcript on `*_delta` events. Keep
/// the Rust state machine's separate block buffer, but copy it into the
/// snapshot at the event boundary rather than only at stream completion.
fn partial_with_blocks(output: &AssistantMessage, blocks: &[ContentBlock]) -> AssistantMessage {
    let mut partial = output.clone();
    let AssistantMessage::Assistant { content, .. } = &mut partial;
    *content = blocks.to_vec();
    partial
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    // The variant is fixed: Assistant.
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

/// Process one `GenerateContentResponse` chunk (the `data:` payload of one
/// SSE event), pushing events into `output`. Returns Result for the
/// terminal-error path (upstream throws mid-loop on some conditions).
fn process_chunk(
    model: &Model,
    chunk: &Value,
    state: &mut GoogleStreamState,
    push: &mut dyn FnMut(AssistantMessageEvent),
) -> Result<(), String> {
    let GoogleStreamState {
        output,
        block_kind,
        blocks,
    } = state;

    // responseId: keep the first non-empty one.
    if output.response_id().is_none() {
        if let Some(id) = chunk.get("responseId").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                output.set_response_id(id.to_string());
            }
        }
    }

    let Some(candidate) = chunk.get("candidates").and_then(|c| c.get(0)) else {
        // No candidates — nothing to stream this chunk.
        if let Some(usage) = chunk.get("usageMetadata") {
            apply_usage(model, output, usage);
        }
        return Ok(());
    };

    if let Some(content) = candidate.get("content") {
        if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
            for part in parts {
                if part.get("text").is_some() {
                    let is_thinking = is_thinking_part(part);
                    let kind = if is_thinking {
                        BlockKind::Thinking
                    } else {
                        BlockKind::Text
                    };
                    let block_kind_equal = block_kind.as_ref() == Some(&kind);
                    if !block_kind_equal {
                        // Close the previous block.
                        if let Some(prev) = block_kind {
                            close_block(*prev, blocks, push);
                            match prev {
                                BlockKind::Text => {
                                    push(AssistantMessageEvent::TextEnd {
                                        content_index: blocks.len() - 1,
                                        content: block_text(blocks, *prev),
                                        partial: partial_with_blocks(output, blocks),
                                    });
                                }
                                BlockKind::Thinking => {
                                    push(AssistantMessageEvent::ThinkingEnd {
                                        content_index: blocks.len() - 1,
                                        content: block_thinking(blocks, *prev),
                                        partial: partial_with_blocks(output, blocks),
                                    });
                                }
                            }
                        }
                        // Start the new block.
                        if is_thinking {
                            blocks.push(ContentBlock::thinking(""));
                            *block_kind = Some(BlockKind::Thinking);
                            push(AssistantMessageEvent::ThinkingStart {
                                content_index: blocks.len() - 1,
                                partial: partial_with_blocks(output, blocks),
                            });
                        } else {
                            blocks.push(ContentBlock::text(""));
                            *block_kind = Some(BlockKind::Text);
                            push(AssistantMessageEvent::TextStart {
                                content_index: blocks.len() - 1,
                                partial: partial_with_blocks(output, blocks),
                            });
                        }
                    }
                    let delta = part
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let signature = part.get("thoughtSignature").and_then(|v| v.as_str());
                    match block_kind.as_ref() {
                        Some(BlockKind::Thinking) => {
                            if let Some(ContentBlock::Thinking {
                                thinking,
                                thinking_signature,
                                ..
                            }) = blocks.last_mut()
                            {
                                *thinking += &delta;
                                *thinking_signature = retain_thought_signature(
                                    thinking_signature.as_deref(),
                                    signature,
                                );
                            }
                            push(AssistantMessageEvent::ThinkingDelta {
                                content_index: blocks.len() - 1,
                                delta,
                                partial: partial_with_blocks(output, blocks),
                            });
                        }
                        _ => {
                            if let Some(ContentBlock::Text {
                                text,
                                text_signature,
                            }) = blocks.last_mut()
                            {
                                *text += &delta;
                                *text_signature =
                                    retain_thought_signature(text_signature.as_deref(), signature);
                            }
                            push(AssistantMessageEvent::TextDelta {
                                content_index: blocks.len() - 1,
                                delta,
                                partial: partial_with_blocks(output, blocks),
                            });
                        }
                    }
                }

                if part.get("functionCall").is_some() {
                    // Close any open text/thinking block.
                    if let Some(prev) = block_kind {
                        close_block(*prev, blocks, push);
                        match prev {
                            BlockKind::Text => {
                                push(AssistantMessageEvent::TextEnd {
                                    content_index: blocks.len() - 1,
                                    content: block_text(blocks, *prev),
                                    partial: partial_with_blocks(output, blocks),
                                });
                            }
                            BlockKind::Thinking => {
                                push(AssistantMessageEvent::ThinkingEnd {
                                    content_index: blocks.len() - 1,
                                    content: block_thinking(blocks, *prev),
                                    partial: partial_with_blocks(output, blocks),
                                });
                            }
                        }
                        *block_kind = None;
                    }

                    let Some(function_call) = part.get("functionCall") else {
                        continue;
                    };
                    let provided_id = function_call.get("id").and_then(|v| v.as_str());
                    let name = function_call
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = function_call.get("args").cloned().unwrap_or(json!({}));
                    let needs_new_id = provided_id.is_none()
                        || blocks
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if Some(id.as_str()) == provided_id));
                    let tool_call_id = if needs_new_id {
                        format!(
                            "{name}_{}_{}",
                            crate::types::now_ms(),
                            next_tool_call_counter()
                        )
                    } else {
                        // provided_id presence was required by the branch above.
                        provided_id.unwrap_or_default().to_string()
                    };
                    let thought_sig = part.get("thoughtSignature").and_then(|v| v.as_str());

                    let mut tool_call = ContentBlock::ToolCall {
                        id: tool_call_id,
                        name,
                        arguments: args,
                        thought_signature: None,
                        namespace: None,
                    };
                    if let Some(sig) = thought_sig {
                        if let ContentBlock::ToolCall {
                            thought_signature, ..
                        } = &mut tool_call
                        {
                            *thought_signature = Some(sig.to_string());
                        }
                    }
                    let tool_args = match &tool_call {
                        ContentBlock::ToolCall { arguments, .. } => arguments.clone(),
                        _ => json!({}),
                    };
                    blocks.push(tool_call.clone());
                    push(AssistantMessageEvent::ToolCallStart {
                        content_index: blocks.len() - 1,
                        partial: partial_with_blocks(output, blocks),
                    });
                    let delta = serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".into());
                    push(AssistantMessageEvent::ToolCallDelta {
                        content_index: blocks.len() - 1,
                        delta,
                        partial: partial_with_blocks(output, blocks),
                    });
                    push(AssistantMessageEvent::ToolCallEnd {
                        content_index: blocks.len() - 1,
                        tool_call,
                        partial: partial_with_blocks(output, blocks),
                    });
                }
            }
        }
    }

    if let Some(finish_reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
        output.set_raw_stop_reason(finish_reason.to_string());
        let stop = map_stop_reason(Some(finish_reason));
        output.set_stop_reason(stop);
        if blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            && stop == StopReason::Stop
        {
            output.set_stop_reason(StopReason::ToolUse);
        }
    }

    if let Some(usage) = chunk.get("usageMetadata") {
        apply_usage(model, output, usage);
    }

    Ok(())
}

fn close_block(
    kind: BlockKind,
    _blocks: &mut Vec<ContentBlock>,
    _push: &mut dyn FnMut(AssistantMessageEvent),
) {
    // Blocks are closed by the caller emitting the matching _end event. This
    // helper exists to mirror the upstream's explicit currentBlock close.
    let _ = kind;
}

fn block_text(blocks: &[ContentBlock], _kind: BlockKind) -> String {
    match blocks.last() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

fn block_thinking(blocks: &[ContentBlock], _kind: BlockKind) -> String {
    match blocks.last() {
        Some(ContentBlock::Thinking { thinking, .. }) => thinking.clone(),
        _ => String::new(),
    }
}

/// Apply usageMetadata to the output message and calculate cost (upstream
/// usage assembly in the loops).
fn apply_usage(model: &Model, output: &mut AssistantMessage, usage: &Value) {
    let prompt = usage
        .get("promptTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached = usage
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let candidates = usage
        .get("candidatesTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let thoughts = usage
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total = usage
        .get("totalTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let mut u = Usage {
        input: prompt.saturating_sub(cached),
        output: candidates + thoughts,
        cache_read: cached,
        cache_write: 0,
        reasoning: Some(thoughts),
        total_tokens: total,
        cache_write_1h: None,
        cost: Default::default(),
    };
    let cost = calculate_cost(model, &u);
    u.cost = cost;
    output.set_usage(u);
}

/// Process all buffered SSE data events through `process_chunk`.
pub fn process_google_events(
    model: &Model,
    events: &[crate::sse::SseEvent],
    mut push: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    let mut state = GoogleStreamState {
        output: new_output(model),
        block_kind: None,
        blocks: Vec::new(),
    };
    for event in events {
        if event.data.trim().is_empty() || event.data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data)
            .map_err(|e| format!("Malformed Google stream chunk: {e}"))?;
        process_chunk(model, &chunk, &mut state, &mut push)?;
    }
    // Close the final open block.
    if let Some(prev) = state.block_kind {
        match prev {
            BlockKind::Text => {
                push(AssistantMessageEvent::TextEnd {
                    content_index: state.blocks.len() - 1,
                    content: block_text(&state.blocks, prev),
                    partial: partial_with_blocks(&state.output, &state.blocks),
                });
            }
            BlockKind::Thinking => {
                push(AssistantMessageEvent::ThinkingEnd {
                    content_index: state.blocks.len() - 1,
                    content: block_thinking(&state.blocks, prev),
                    partial: partial_with_blocks(&state.output, &state.blocks),
                });
            }
        }
    }
    // Sync the assembled blocks back into output (the pusher clones carried
    // the live output; ensure the final message carries them too).
    if !state.blocks.is_empty() {
        let AssistantMessage::Assistant { content, .. } = &mut state.output;
        if content.is_empty() {
            *content = state.blocks.clone();
        }
    }
    match state.output.stop_reason() {
        Some(StopReason::Pending) => Err("Google stream ended without a finish reason".to_string()),
        Some(StopReason::Aborted) | Some(StopReason::Error) => {
            let raw = state.output.raw_stop_reason().unwrap_or("").to_string();
            let msg = if !raw.is_empty() {
                format!("Provider stopped with: {raw}")
            } else {
                "An unknown error occurred".to_string()
            };
            Err(msg)
        }
        _ => Ok(state.output),
    }
}

// ---------------------------------------------------------------------------
// stream / streamSimple
// ---------------------------------------------------------------------------

fn replace_header(headers: &mut ProviderHeaders, name: impl Into<String>, value: Option<String>) {
    let name = name.into();
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
    headers.insert(name, value);
}

fn build_request_headers(
    model: &Model,
    options_headers: Option<&ProviderHeaders>,
    api_key: Option<&str>,
) -> ProviderHeaders {
    let mut headers = ProviderHeaders::new();
    replace_header(
        &mut headers,
        "User-Agent",
        Some(super::mistral_conversations::pi_user_agent()),
    );
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            replace_header(&mut headers, name.clone(), Some(value.clone()));
        }
    }
    if let Some(api_key) = api_key {
        replace_header(&mut headers, "x-goog-api-key", Some(api_key.to_string()));
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            replace_header(&mut headers, name.clone(), value.clone());
        }
    }
    headers
}

/// Streams a request against the Google Generative AI REST endpoint.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &GoogleOptions,
) -> AssistantMessageEventStream {
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return immediate_error_stream(model, "Request was aborted", true);
    }
    // Upstream throws synchronously when the api key is absent; the port
    // encodes the same failure as an immediate terminal error event (stream).
    if api_key.is_none()
        && crate::api::openai_completions::get_provider_env_value(
            "GEMINI_API_KEY",
            options.base.base.env.as_ref(),
        )
        .is_none()
    {
        let mut message = new_output(model);
        message.set_stop_reason(StopReason::Error);
        super::anthropic_messages::set_error_message(
            &mut message,
            format!("No API key for provider: {}", model.provider),
        );
        return crate::event_stream::create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            message.error_message().unwrap_or("").to_string(),
        );
    }
    let stream = AssistantMessageEventStream::new();
    let Some(sender) = stream.sender() else {
        return stream;
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let api_key = api_key.map(|s| s.to_string()).or_else(|| {
        crate::api::openai_completions::get_provider_env_value(
            "GEMINI_API_KEY",
            options.base.base.env.as_ref(),
        )
    });
    let base_url = base_url.to_string();

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        if signal_aborted(options.base.abort_signal.as_ref()) {
            let message = terminal_error_message(&model, "Request was aborted", true);
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        let params = match build_params(&model, &context, &options) {
            Ok(params) => params,
            Err(error) => {
                let message = terminal_error_message(&model, error, false);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let params = match apply_payload_hook(
            params,
            &model,
            options.base.on_payload.as_ref(),
            options.base.abort_signal.clone(),
        )
        .await
        {
            Ok(params) => params,
            Err(_) => {
                let message = terminal_error_message(&model, "Request was aborted", true);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };

        let endpoint = format!(
            "{base_url}/models/{}:streamGenerateContent?alt=sse",
            model.id
        );
        let headers = build_request_headers(
            &model,
            options.base.base.headers.as_ref(),
            api_key.as_deref(),
        );
        let mut request = client
            .post(&endpoint)
            .header("content-type", "application/json")
            .json(&params);
        for (name, value) in headers {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }

        let response = match abortable(request.send(), options.base.abort_signal.clone()).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                let message =
                    terminal_error_message(&model, format!("Request failed: {err}"), false);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
            Err(_) => {
                let message = terminal_error_message(&model, "Request was aborted", true);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let status = response.status();
        let provider_response = crate::types::ProviderResponse {
            status: status.as_u16(),
            headers: crate::utils::response_headers(response.headers()),
        };
        if let Some(on_response) = &options.base.on_response {
            on_response(&provider_response, &model);
        }
        let body = match abortable(response.bytes(), options.base.abort_signal.clone()).await {
            Ok(Ok(body)) => body,
            Ok(Err(err)) => {
                let message =
                    terminal_error_message(&model, format!("Request body failed: {err}"), false);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
            Err(_) => {
                let message = terminal_error_message(&model, "Request was aborted", true);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        if signal_aborted(options.base.abort_signal.as_ref()) {
            let message = terminal_error_message(&model, "Request was aborted", true);
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        if !status.is_success() {
            let body_text = String::from_utf8_lossy(&body).to_string();
            let detail = extract_google_error(&body_text);
            let message = terminal_error_message(
                &model,
                format!("Google API error ({}): {}", status.as_u16(), detail),
                false,
            );
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }

        let body_text = String::from_utf8_lossy(&body).to_string();
        let events = SseParser::parse_text(&body_text);

        pusher.push(AssistantMessageEvent::Start {
            partial: new_output(&model),
        });
        match process_google_events(&model, &events, |event| pusher.push(event)) {
            Ok(message) => {
                if signal_aborted(options.base.abort_signal.as_ref()) {
                    let message = terminal_error_message(&model, "Request was aborted", true);
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Aborted,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                    return;
                }
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
                let aborted = signal_aborted(options.base.abort_signal.as_ref());
                let message = terminal_error_message(
                    &model,
                    if aborted {
                        "Request was aborted".to_string()
                    } else {
                        err
                    },
                    aborted,
                );
                pusher.push(AssistantMessageEvent::Error {
                    reason: error_reason(aborted),
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
            }
        }
    });
    std::mem::forget(handle);
    stream
}

/// Pull the Google error message from an error response body.
pub fn extract_google_error(body: &str) -> String {
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

/// Simple stream: resolves reasoning to a Google thinking config and forwards
/// to `stream` (upstream `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let Some(api_key) = api_key.map(|s| s.to_string()).or_else(|| {
        crate::api::openai_completions::get_provider_env_value(
            "GEMINI_API_KEY",
            options.base.base.env.as_ref(),
        )
    }) else {
        let mut message = new_output(model);
        message.set_stop_reason(StopReason::Error);
        super::anthropic_messages::set_error_message(
            &mut message,
            format!("No API key for provider: {}", model.provider),
        );
        return crate::event_stream::create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            message.error_message().unwrap_or("").to_string(),
        );
    };

    if options.reasoning.is_none() {
        let mut base = options.clone();
        base.reasoning = None;
        let go = GoogleOptions {
            base: base.base.clone(),
            tool_choice: options.tool_choice.as_ref().map(|t| match t {
                ToolChoice::Auto => "auto".into(),
                ToolChoice::None => "none".into(),
            }),
            thinking: Some(GoogleThinking {
                enabled: false,
                budget_tokens: None,
                level: None,
            }),
        };
        return stream(model, context, client, base_url, Some(&api_key), &go);
    }

    #[allow(clippy::expect_used)] // invariant: callers resolve reasoning before this path
    let reasoning = options
        .reasoning
        .expect("reasoning resolved before thinking clamp");
    let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(reasoning));
    let resolved = resolve_google_thinking_level(clamped, model);
    let model_id = model.id.clone();

    let thinking = if is_gemini3_pro_model(&model_id)
        || is_gemini3_flash_model(&model_id)
        || is_gemma4_model(&model_id)
    {
        GoogleThinking {
            enabled: true,
            budget_tokens: None,
            level: Some(google_thinking_level(resolved, &model_id)),
        }
    } else {
        GoogleThinking {
            enabled: true,
            budget_tokens: Some(google_budget(
                &model_id,
                resolved,
                options.thinking_budgets.as_ref(),
            )),
            level: None,
        }
    };

    let go = GoogleOptions {
        base: options.base.clone(),
        tool_choice: options.tool_choice.as_ref().map(|t| match t {
            ToolChoice::Auto => "auto".into(),
            ToolChoice::None => "none".into(),
        }),
        thinking: Some(thinking),
    };
    stream(model, context, client, base_url, Some(&api_key), &go)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::{Model, ModelInput};
    use crate::types::*;

    fn model(id: &str) -> Model {
        let mut m = Model::new(id, id, "google-generative-ai", "google");
        m.base_url = DEFAULT_BASE_URL.to_string();
        m.reasoning = true;
        m.input = vec![ModelInput::Text, ModelInput::Image];
        m
    }

    fn ctx() -> Context {
        Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![Message::User(UserContent::string("hello", 1))],
            tools: vec![json_tool(
                "bash",
                "run a command",
                &json!({"type":"object","properties":{}}),
            )],
        }
    }

    #[test]
    fn build_params_has_contents_system_and_tools() {
        let m = model("gemini-2.5-pro");
        let params = build_params(
            &m,
            &ctx(),
            &GoogleOptions::from_stream_options(StreamOptions::default()),
        )
        .unwrap();
        assert_eq!(params["contents"][0]["role"], "user");
        assert_eq!(
            params["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );
        assert_eq!(
            params["tools"][0]["functionDeclarations"][0]["name"],
            "bash"
        );
    }

    #[test]
    fn build_params_uses_validated_mode_for_strict_sampling() {
        let m = model("gemini-3-pro");
        let mut context = ctx();
        context.tools[0].parameters = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        context.tools[0].constrained_sampling = Some(ConstrainedSampling::JsonSchema {
            strict: StrictPreference::Prefer,
        });
        let params = build_params(
            &m,
            &context,
            &GoogleOptions::from_stream_options(StreamOptions::default()),
        )
        .unwrap();
        assert_eq!(
            params["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        assert_eq!(
            params["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"],
            json!(["path"])
        );
        assert_eq!(
            params["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]
                ["additionalProperties"],
            false
        );
    }

    #[test]
    fn build_params_rejects_required_unsupported_schema() {
        let m = model("gemini-3-pro");
        let mut context = ctx();
        context.tools[0].parameters = json!({
            "type": "object",
            "properties": {},
            "allOf": []
        });
        context.tools[0].constrained_sampling = Some(ConstrainedSampling::JsonSchema {
            strict: StrictPreference::Require,
        });
        assert_eq!(
            build_params(
                &m,
                &context,
                &GoogleOptions::from_stream_options(StreamOptions::default()),
            )
            .unwrap_err(),
            "Tool \"bash\" requires JSON-schema constrained sampling, but allOf schemas are unsupported."
        );
    }

    #[test]
    fn build_params_thinking_budget_for_25_pro() {
        let m = model("gemini-2.5-pro");
        let opts = GoogleOptions {
            base: StreamOptions::default(),
            tool_choice: None,
            thinking: Some(GoogleThinking {
                enabled: true,
                budget_tokens: Some(8192),
                level: None,
            }),
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(params["thinkingConfig"]["includeThoughts"], true);
        assert_eq!(params["thinkingConfig"]["thinkingBudget"], 8192);
    }

    #[test]
    fn build_params_thinking_level_for_gemini3_pro() {
        let m = model("gemini-3-pro");
        let opts = GoogleOptions {
            base: StreamOptions::default(),
            tool_choice: None,
            thinking: Some(GoogleThinking {
                enabled: true,
                budget_tokens: None,
                level: Some("HIGH"),
            }),
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(params["thinkingConfig"]["thinkingLevel"], "HIGH");
    }

    #[test]
    fn disabled_thinking_config_by_model_family() {
        assert_eq!(
            disabled_thinking_config("gemini-3-pro"),
            json!({"thinkingLevel":"LOW"})
        );
        assert_eq!(
            disabled_thinking_config("gemini-3-flash"),
            json!({"thinkingLevel":"MINIMAL"})
        );
        assert_eq!(
            disabled_thinking_config("gemma-4-27b"),
            json!({"thinkingLevel":"MINIMAL"})
        );
        assert_eq!(
            disabled_thinking_config("gemini-2.5-pro"),
            json!({"thinkingBudget":0})
        );
    }

    #[test]
    fn google_budget_tables() {
        assert_eq!(
            google_budget("gemini-2.5-pro", ResolvedGoogleThinkingLevel::Medium, None),
            8192
        );
        assert_eq!(
            google_budget("gemini-2.5-flash", ResolvedGoogleThinkingLevel::High, None),
            24576
        );
        // Unknown family -> dynamic (-1)
        assert_eq!(
            google_budget("gemini-1.5-pro", ResolvedGoogleThinkingLevel::High, None),
            -1
        );
        // Custom budgets override.
        let custom = ThinkingBudgets {
            minimal: Some(10),
            low: Some(20),
            medium: Some(30),
            high: Some(40),
        };
        assert_eq!(
            google_budget(
                "gemini-2.5-pro",
                ResolvedGoogleThinkingLevel::Low,
                Some(&custom)
            ),
            20
        );
    }

    #[test]
    fn process_chunk_streams_text_and_usage() {
        let m = model("gemini-2.5-pro");
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "Hello" }, { "text": " world" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let mut state = GoogleStreamState {
            output: new_output(&m),
            block_kind: None,
            blocks: Vec::new(),
        };
        process_chunk(&m, &chunk, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(state.output.stop_reason(), Some(StopReason::Stop));
        let text: String = state
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        let usage = state.output.usage().unwrap();
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5);
    }

    #[test]
    fn process_chunk_tool_call_is_tool_use() {
        let m = model("gemini-3-pro");
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [{
                    "functionCall": { "name": "bash", "args": {"cmd": "ls"}, "id": "fc-1" }
                }] },
                "finishReason": "STOP"
            }]
        });
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let mut state = GoogleStreamState {
            output: new_output(&m),
            block_kind: None,
            blocks: Vec::new(),
        };
        process_chunk(&m, &chunk, &mut state, &mut |e| events.push(e)).unwrap();
        // With a tool call present, STOP maps to toolUse
        assert_eq!(state.output.stop_reason(), Some(StopReason::ToolUse));
        match &state.blocks[0] {
            ContentBlock::ToolCall { id, name, .. } => {
                assert_eq!(id, "fc-1");
                assert_eq!(name, "bash");
            }
            b => panic!("expected toolCall: {b:?}"),
        }
    }

    #[test]
    fn process_chunk_thinking_delta() {
        let m = model("gemini-2.5-pro");
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "reasoning...", "thought": true }, { "text": "answer" }] }
            }],
            "usageMetadata": { "promptTokenCount": 3 }
        });
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let mut state = GoogleStreamState {
            output: new_output(&m),
            block_kind: None,
            blocks: Vec::new(),
        };
        process_chunk(&m, &chunk, &mut state, &mut |e| events.push(e)).unwrap();
        assert!(matches!(state.blocks[0], ContentBlock::Thinking { .. }));
        assert!(matches!(state.blocks[1], ContentBlock::Text { .. }));
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::ThinkingDelta { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextStart { .. })));
    }

    #[test]
    fn streamed_partials_include_live_text_and_tool_blocks() {
        let m = model("gemini-2.5-pro");
        let events = vec![
            crate::sse::SseEvent {
                data: json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "answer" }] }
                    }]
                })
                .to_string(),
                event: None,
                id: None,
            },
            crate::sse::SseEvent {
                data: json!({
                    "candidates": [{
                        "content": { "parts": [{
                            "functionCall": {
                                "name": "lookup",
                                "args": { "key": "value" },
                                "id": "call-1"
                            }
                        }] },
                        "finishReason": "STOP"
                    }]
                })
                .to_string(),
                event: None,
                id: None,
            },
        ];
        let mut seen_text = false;
        let mut seen_tool = false;
        let result = process_google_events(&m, &events, |event| match event {
            AssistantMessageEvent::TextDelta { partial, .. } => {
                seen_text = partial.content().iter().any(
                    |block| matches!(block, ContentBlock::Text { text, .. } if text == "answer"),
                );
            }
            AssistantMessageEvent::ToolCallStart { partial, .. } => {
                seen_tool = partial.content().iter().any(
                    |block| matches!(block, ContentBlock::ToolCall { id, .. } if id == "call-1"),
                );
            }
            _ => {}
        })
        .expect("fixture has a terminal finish reason");

        assert!(seen_text, "text delta partial lost the current text block");
        assert!(seen_tool, "tool-call partial lost the current tool block");
        assert!(result
            .content()
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { id, .. } if id == "call-1")));
    }

    #[test]
    fn missing_finish_reason_is_error() {
        let m = model("gemini-2.5-pro");
        let events = vec![crate::sse::SseEvent {
            data: "{}".into(),
            event: None,
            id: None,
        }];
        let result = process_google_events(&m, &events, |_| {});
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("without a finish reason"));
    }

    #[test]
    fn error_finish_reason_surfaces_message() {
        let m = model("gemini-2.5-pro");
        let chunk = json!({ "candidates": [{ "finishReason": "SAFETY" }] });
        let events = vec![crate::sse::SseEvent {
            data: chunk.to_string(),
            event: None,
            id: None,
        }];
        let result = process_google_events(&m, &events, |_| {});
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Provider stopped with: SAFETY"), "{err}");
    }

    #[test]
    fn stream_simple_no_reasoning_disables_thinking() {
        // No network: this just verifies stream_simple constructs a valid
        // error stream when the key is absent (key check precedes network).
        let m = model("gemini-2.5-pro");
        let _guard = crate::utils::env_lock();
        std::env::remove_var("GEMINI_API_KEY");
        let opts = SimpleStreamOptions {
            base: StreamOptions::default(),
            tool_choice: None,
            reasoning: None,
            deferred: None,
            thinking_budgets: None,
        };
        let stream = stream_simple(
            &m,
            &Context::default(),
            reqwest::Client::new(),
            DEFAULT_BASE_URL,
            None,
            &opts,
        );
        // The stream must produce a terminal error quickly.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, msg) = rt.block_on(stream.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        assert!(msg.error_message().is_some());
    }
}
