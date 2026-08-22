//! OpenAI Codex Responses API adaptor — port of
//! `packages/ai/src/api/openai-codex-responses.ts`.
//!
//! Streams the ChatGPT Codex `POST /codex/responses` SSE endpoint with the
//! shared OpenAI Responses request/event machinery (developer-less system
//! prompt -> `instructions`, `prompt_cache_key` session affinity, reasoning
//! effort through the model thinking-level map, service-tier cost
//! multipliers, Codex-`error`/`response.failed` event errors, `end_turn`
//! passthrough, and terminal-event stop reasons). `stream` never throws:
//! failures are encoded as a terminal error event.
//!
//! Divergences (documented):
//! - WebSocket transport (upstream default `transport: "auto"` tries a
//!   session-cached WebSocket before falling back to SSE) is not implemented;
//!   requests always use the SSE path, which is exactly the upstream fallback
//!   when a runtime has no `WebSocket` global (as Rust has none). Cached
//!   delta-request continuation is therefore unavailable.
//! - zstd request-body compression is not implemented; bodies are sent
//!   uncompressed (the upstream helper already returns null in runtimes
//!   without `node:zlib`).
//! - `options.signal` is not part of the ported `StreamOptions`, so
//!   user-initiated aborts cannot be plumbed through `stream`; the header
//!   timeout (`options.base.timeoutMs`) still bounds the initial fetch, and
//!   the SSE body is read incrementally until a terminal event arrives (or
//!   the body ends), mirroring upstream cancellation after
//!   `response.completed`.
//! - The upstream `onPayload` hook is not part of the ported `StreamOptions`
//!   surface; payload mutation hooks are unavailable.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use futures_util::StreamExt;

use crate::model::{clamp_thinking_level, Model};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, Tool, ToolChoice, Usage,
};
use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::sse::{SseEvent, SseParser};

use super::mistral_conversations::pi_user_agent;
use super::openai_responses_shared::*;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];

const CODEX_RESPONSE_STATUSES: [&str; 6] =
    ["completed", "incomplete", "failed", "cancelled", "queued", "in_progress"];

/// Provider-specific options for the Codex Responses API (upstream
/// `OpenAICodexResponsesOptions`, reduced to the fields the ported
/// `StreamOptions` currently carries).
#[derive(Clone, Default)]
pub struct OpenAICodexResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<Value>,
}

/// Clamp a session id to OpenAI's 64-char prompt-cache limit (upstream
/// `clampOpenAIPromptCacheKey`).
fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|k| k.chars().take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH).collect())
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

/// Resolve the Codex SSE endpoint from a base URL (upstream
/// `resolveCodexUrl`).
fn resolve_codex_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CODEX_BASE_URL)
        .to_string();
    if raw.ends_with("/codex/responses") {
        return raw;
    }
    if raw.ends_with("/codex") {
        return format!("{raw}/responses");
    }
    format!("{raw}/codex/responses")
}

// ---------------------------------------------------------------------------
// Auth & headers
// ---------------------------------------------------------------------------

/// Extract the ChatGPT account id from a Codex access token's JWT claims
/// (upstream `extractAccountId`).
fn extract_account_id(token: &str) -> Result<String, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("Failed to extract accountId from token".to_string());
    }
    use base64::Engine;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(parts[1])
        .map_err(|_| "Failed to extract accountId from token".to_string());
    let payload = payload?;
    let parsed: Value = serde_json::from_slice(&payload)
        .map_err(|_| "Failed to extract accountId from token".to_string())?;
    let account_id = parsed
        .get(JWT_CLAIM_PATH)
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str());
    match account_id {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err("Failed to extract accountId from token".to_string()),
    }
}

/// Build the shared Codex request headers (upstream `buildBaseCodexHeaders`
/// + `buildSSEHeaders`).
fn build_codex_headers(
    model_headers: Option<&BTreeMap<String, String>>,
    additional_headers: Option<&crate::types::ProviderHeaders>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    if let Some(model_headers) = model_headers {
        for (name, value) in model_headers {
            headers.insert(name.to_lowercase(), value.clone());
        }
    }
    if let Some(additional) = additional_headers {
        for (name, value) in additional {
            let lower = name.to_lowercase();
            match value {
                None => {
                    headers.remove(&lower);
                }
                Some(value) => {
                    headers.insert(lower, value.clone());
                }
            }
        }
    }
    headers.insert("authorization".to_string(), format!("Bearer {token}"));
    headers.insert("chatgpt-account-id".to_string(), account_id.to_string());
    headers.insert("originator".to_string(), "pi".to_string());
    headers.insert("user-agent".to_string(), pi_user_agent());
    headers.insert("openai-beta".to_string(), "responses=experimental".to_string());
    headers.insert("accept".to_string(), "text/event-stream".to_string());
    headers.insert("content-type".to_string(), "application/json".to_string());
    if let Some(session_id) = session_id {
        headers.insert("session-id".to_string(), session_id.to_string());
        headers.insert("x-client-request-id".to_string(), session_id.to_string());
    }
    headers
}

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// Codex-specific tool conversion (upstream `convertResponsesTools` with
/// `{ strict: null }`): unconstrained tools carry an explicit `strict: null`,
/// constrained `prefer`/`require` tools resolve through the strict JSON-schema
/// converter, and the strict field is only emitted when strict mode applies.
fn convert_codex_tools(tools: &[Tool], supports_strict_mode: bool, supports_openai_grammar_tools: bool) -> Result<Vec<Value>, String> {
    let default_strict = Value::Null;
    let mut result: Vec<Value> = Vec::new();
    for tool in tools {
        // Grammar tools are not ported (upstream resolves them to `custom`
        // tools when the model supports them). Documented divergence.
        let _ = supports_openai_grammar_tools;
        let constrained_strict = super::mistral_conversations::resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
        let strict = match constrained_strict {
            Some(v) => Value::Bool(v),
            None => default_strict.clone(),
        };
        let parameters = if constrained_strict == Some(true) {
            super::mistral_conversations::make_strict_json_schema(&tool.parameters)?
        } else {
            tool.parameters.clone()
        };
        let mut function_tool = json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": parameters,
        });
        if supports_strict_mode {
            function_tool["strict"] = strict;
        }
        result.push(function_tool);
    }
    Ok(result)
}

/// Assemble the Codex request body (port of `buildRequestBody`).
fn build_request_body(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    cache_session_id: Option<&str>,
) -> Result<Value, String> {
    let compat = model.compat.as_ref();
    let supports_strict_mode = compat.and_then(|c| c.get("supportsStrictMode")).and_then(|v| v.as_bool()).unwrap_or(true);
    let supports_openai_grammar_tools = compat.and_then(|c| c.get("supportsOpenAIGrammarTools")).and_then(|v| v.as_bool()).unwrap_or(false);

    let messages = convert_responses_messages(
        model,
        context,
        &CODEX_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions { include_system_prompt: false },
    );

    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": context.system_prompt.clone().unwrap_or_else(|| "You are a helpful assistant.".to_string()),
        "input": messages,
        "text": { "verbosity": options.text_verbosity.clone().unwrap_or_else(|| "low".to_string()) },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": options.tool_choice.clone().unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": true,
    });
    if let Some(cache_session_id) = cache_session_id {
        body["prompt_cache_key"] = json!(cache_session_id);
    }
    if let Some(temperature) = options.base.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        body["service_tier"] = json!(service_tier);
    }
    if !context.tools.is_empty() {
        body["tools"] = json!(convert_codex_tools(&context.tools, supports_strict_mode, supports_openai_grammar_tools)?);
    }
    if let Some(reasoning_effort) = &options.reasoning_effort {
        // Upstream maps the requested effort through the model's
        // thinking-level map first (e.g. minimal -> low for Codex models),
        // defaulting to the requested string. The "none" effort maps to the
        // model's off entry or the sentinel "none".
        let effort = if reasoning_effort == "none" {
            model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::Off))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_string())
        } else {
            model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::from_effort_str(reasoning_effort)))
                .cloned()
                .flatten()
                .unwrap_or_else(|| reasoning_effort.clone())
        };
        body["reasoning"] = json!({
            "effort": effort,
            "summary": options.reasoning_summary.clone().unwrap_or_else(|| "auto".to_string()),
        });
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Retry helpers
// ---------------------------------------------------------------------------

fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    regex::RegexBuilder::new(
        r"GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing",
    )
    .case_insensitive(true)
    .build()
    .expect("terminal rate-limit regex must compile")
    .is_match(error_text)
}

fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if status == 429 || (500..=504).contains(&status) {
        return true;
    }
    regex::RegexBuilder::new(r"rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused")
        .case_insensitive(true)
        .build()
        .expect("retryable regex must compile")
        .is_match(error_text)
}

/// Read retry-after guidance from response headers (upstream
/// `getRetryAfterDelayMs`). HTTP-date `Retry-After` values are not parsed
/// (documented divergence); the numeric forms are.
fn get_retry_after_delay_ms(headers: &BTreeMap<String, String>) -> Option<u64> {
    if let Some(retry_after_ms) = headers.get("retry-after-ms") {
        if let Ok(millis) = retry_after_ms.parse::<f64>() {
            if millis.is_finite() {
                return Some(millis.max(0.0) as u64);
            }
        }
    }
    let retry_after = headers.get("retry-after")?;
    if let Ok(seconds) = retry_after.parse::<f64>() {
        if seconds.is_finite() {
            return Some((seconds.max(0.0) * 1000.0) as u64);
        }
    }
    None
}

fn retry_delay_exceeded_message(delay_ms: u64, max_retry_delay_ms: u64) -> String {
    format!(
        "Server requested {}s retry delay (max: {}s)",
        delay_ms.div_ceil(1000),
        max_retry_delay_ms.div_ceil(1000)
    )
}

/// Validate a server-requested retry delay against the caller's max
/// (upstream `validateRetryDelayMs`, returns Err on exceeded).
fn validate_retry_delay_ms(delay_ms: u64, base: &StreamOptions) -> Result<u64, String> {
    let max_retry_delay_ms = base.base.max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_retry_delay_ms > 0 && delay_ms > max_retry_delay_ms {
        return Err(retry_delay_exceeded_message(delay_ms, max_retry_delay_ms));
    }
    Ok(delay_ms)
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

// ---------------------------------------------------------------------------
// Error response parsing
// ---------------------------------------------------------------------------

/// Parse a Codex error response into `(message, friendlyMessage)` (upstream
/// `parseErrorResponse`).
fn parse_error_response(raw: &str, status: u16, status_text: &str) -> (String, Option<String>) {
    let mut message = if raw.is_empty() {
        if status_text.is_empty() {
            "Request failed".to_string()
        } else {
            status_text.to_string()
        }
    } else {
        raw.to_string()
    };
    let mut friendly_message: Option<String> = None;

    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        let err = parsed.get("error").cloned().unwrap_or(Value::Null);
        if err.is_object() {
            let code = err
                .get("code")
                .and_then(|v| v.as_str())
                .or_else(|| err.get("type").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            let usage_limit = regex::RegexBuilder::new(r"usage_limit_reached|usage_not_included|rate_limit_exceeded")
                .case_insensitive(true)
                .build()
                .expect("usage-limit regex must compile")
                .is_match(&code);
            if usage_limit || status == 429 {
                let plan = err
                    .get("plan_type")
                    .and_then(|v| v.as_str())
                    .map(|p| format!(" ({} plan)", p.to_lowercase()))
                    .unwrap_or_default();
                let mins = err
                    .get("resets_at")
                    .and_then(|v| v.as_f64())
                    .map(|resets_at| {
                        let now = crate::types::now_ms() as f64;
                        let diff = (resets_at * 1000.0 - now) / 60000.0;
                        diff.round().max(0.0) as u64
                    });
                let when = mins.map(|m| format!(" Try again in ~{m} min.")).unwrap_or_default();
                friendly_message = Some(format!("You have hit your ChatGPT usage limit{plan}.{when}").trim().to_string());
            }
            if let Some(err_message) = err.get("message").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) {
                message = err_message.to_string();
            } else if let Some(friendly) = &friendly_message {
                message = friendly.clone();
            }
        }
    }
    (message, friendly_message)
}

// ---------------------------------------------------------------------------
// Codex event mapping
// ---------------------------------------------------------------------------

fn normalize_codex_status(status: &str) -> Option<String> {
    if CODEX_RESPONSE_STATUSES.contains(&status) {
        Some(status.to_string())
    } else {
        None
    }
}

fn extract_codex_event_error(parsed: &Value) -> (Option<String>, Option<String>) {
    let nested = parsed.get("error").filter(|v| v.is_object());
    let code = parsed
        .get("code")
        .and_then(|v| v.as_str())
        .or_else(|| nested.and_then(|n| n.get("code")).and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let message = parsed
        .get("message")
        .and_then(|v| v.as_str())
        .or_else(|| nested.and_then(|n| n.get("message")).and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    (code, message)
}

/// Resolve the effective service tier when the backend echoes `default`
/// (upstream `resolveCodexServiceTier`).
fn resolve_codex_service_tier(response_tier: Option<&str>, request_tier: Option<&str>) -> Option<String> {
    match response_tier {
        Some("default") if request_tier == Some("flex") || request_tier == Some("priority") => {
            request_tier.map(|s| s.to_string())
        }
        Some(tier) => Some(tier.to_string()),
        None => request_tier.map(|s| s.to_string()),
    }
}

/// Map raw Codex SSE events to normalized `response.completed`-style events
/// for the shared processor (upstream `mapCodexEvents`). Stops at the first
/// terminal event and returns the normalized events so far.
fn map_codex_events(
    events: &[SseEvent],
    output: &mut AssistantMessage,
    request_service_tier: Option<&str>,
) -> Result<Vec<SseEvent>, String> {
    let mut normalized: Vec<SseEvent> = Vec::new();
    for event in events {
        let parsed: Value = serde_json::from_str(&event.data)
            .map_err(|e| format!("Invalid Codex SSE JSON: {e}"))?;
        let event_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if event_type.is_empty() {
            continue;
        }
        match event_type {
            "error" => {
                let (code, message) = extract_codex_event_error(&parsed);
                return Err(format!(
                    "Codex error: {}",
                    message.or(code).unwrap_or_else(|| parsed.to_string())
                ));
            }
            "response.failed" => {
                let response = parsed.get("response").cloned().unwrap_or(Value::Null);
                let error = response.get("error").cloned().unwrap_or(Value::Null);
                let code = error.get("code").and_then(|v| v.as_str());
                let message = error.get("message").and_then(|v| v.as_str());
                let _ = code;
                return Err(message.unwrap_or("Codex response failed").to_string());
            }
            "response.done" | "response.completed" | "response.incomplete" => {
                if let Some(end_turn) = parsed
                    .get("response")
                    .and_then(|r| r.get("end_turn"))
                    .and_then(|v| v.as_bool())
                {
                    let AssistantMessage::Assistant { end_turn: slot, .. } = output;
                    *slot = Some(end_turn);
                }
                let mut mapped = parsed;
                if let Some(response) = mapped.get_mut("response") {
                    if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                        match normalize_codex_status(status) {
                            Some(normalized_status) => {
                                response["status"] = json!(normalized_status);
                            }
                            None => {
                                response.as_object_mut().map(|obj| obj.remove("status"));
                            }
                        }
                    }
                    let response_tier = response.get("service_tier").and_then(|v| v.as_str());
                    if let Some(tier) = resolve_codex_service_tier(response_tier, request_service_tier) {
                        response["service_tier"] = json!(tier);
                    }
                }
                mapped["type"] = json!("response.completed");
                normalized.push(SseEvent {
                    data: mapped.to_string(),
                    event: event.event.clone(),
                    id: event.id.clone(),
                });
                return Ok(normalized);
            }
            _ => normalized.push(event.clone()),
        }
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// SSE reading
// ---------------------------------------------------------------------------

/// Read the Codex SSE body event-by-event until a terminal event arrives
/// (upstream `parseSSE` consumed by `mapCodexEvents`; the reader stops exactly
/// when `response.completed`/`response.incomplete`/`response.done` appears, so
/// a backend that keeps the body open after the terminal event still
/// completes the stream).
async fn read_codex_sse_events(response: reqwest::Response) -> Result<Vec<SseEvent>, String> {
    let mut parser = SseParser::new();
    let mut events = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Codex SSE read failed: {e}"))?;
        for event in parser.push_bytes(&chunk) {
            if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                continue;
            }
            let parsed: Value = serde_json::from_str(&event.data)
                .map_err(|e| format!("Invalid Codex SSE JSON: {e}"))?;
            let is_terminal = parsed
                .get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t == "response.completed" || t == "response.incomplete" || t == "response.done");
            events.push(event);
            if is_terminal {
                return Ok(events);
            }
        }
    }
    events.extend(parser.finish());
    Ok(events)
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

fn set_output_error_message(output: &mut AssistantMessage, message: String) {
    let AssistantMessage::Assistant { error_message, .. } = output;
    *error_message = Some(message);
}

// ---------------------------------------------------------------------------
// Main stream functions
// ---------------------------------------------------------------------------

async fn run_stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: &str,
    options: &OpenAICodexResponsesOptions,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
) -> Result<AssistantMessage, String> {
    let mut output = new_output(model);

    let account_id = extract_account_id(api_key)?;
    // cacheRetention "none" disables prompt-cache affinity entirely
    // (upstream `options?.cacheRetention === "none" ? undefined : options?.sessionId`).
    let cache_session_id = if options.base.cache_retention.as_deref() == Some(crate::types::CACHE_RETENTION_NONE) {
        None
    } else {
        clamp_openai_prompt_cache_key(options.base.session_id.as_deref())
    };
    let body = build_request_body(model, context, options, cache_session_id.as_deref())?;
    let headers = build_codex_headers(
        model.headers.as_ref(),
        options.base.base.headers.as_ref(),
        &account_id,
        api_key,
        cache_session_id.as_deref(),
    );
    let url = resolve_codex_url(Some(&model.base_url));
    let http_timeout_ms = options.base.base.timeout_ms;

    // Build the request once and re-execute per retry attempt.
    let mut builder = client.post(&url);
    {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (name, value) in &headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                header_map.insert(name, value);
            }
        }
        builder = builder.headers(header_map);
    }
    builder = builder.json(&body);
    let request = builder
        .build()
        .map_err(|e| format!("Failed to build Codex request: {e}"))?;

    let max_retries = options
        .base
        .base
        .max_retries
        .unwrap_or(DEFAULT_MAX_RETRIES);
    let mut attempt = 0u32;
    let mut response: Option<reqwest::Response> = None;
    let mut last_error: Option<String> = None;

    while attempt <= max_retries {
        let Some(req) = request.try_clone() else {
            last_error = Some("Codex request body is not cloneable".to_string());
            break;
        };
        // The header timeout bounds only the initial fetch (response headers).
        let send_result = match http_timeout_ms.filter(|t| *t > 0) {
            Some(timeout) => match tokio::time::timeout(
                std::time::Duration::from_millis(timeout),
                client.execute(req),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => {
                    last_error = Some(format!("Codex SSE response headers timed out after {timeout}ms"));
                    break;
                }
            },
            None => client.execute(req).await,
        };

        match send_result {
            Ok(res) => {
                let status = res.status();
                let provider_headers: BTreeMap<String, String> = res
                    .headers()
                    .iter()
                    .map(|(name, value)| (name.as_str().to_lowercase(), value.to_str().unwrap_or("").to_string()))
                    .collect();
                let provider_response = crate::types::ProviderResponse {
                    status: status.as_u16(),
                    headers: provider_headers.clone(),
                };
                if let Some(on_response) = &options.base.on_response {
                    on_response(&provider_response, model);
                }
                if status.is_success() {
                    response = Some(res);
                    break;
                }
                let error_text = res.text().await.unwrap_or_default();
                if attempt < max_retries && is_retryable_error(status.as_u16(), &error_text) {
                    let delay = match get_retry_after_delay_ms(&provider_headers) {
                        Some(delay) => validate_retry_delay_ms(delay, &options.base)?,
                        None => BASE_DELAY_MS * 2u64.pow(attempt),
                    };
                    sleep_ms(delay).await;
                    attempt += 1;
                    continue;
                }
                let status_text = status.canonical_reason().unwrap_or("").to_string();
                let (message, friendly_message) = parse_error_response(&error_text, status.as_u16(), &status_text);
                last_error = Some(friendly_message.unwrap_or(message));
                break;
            }
            Err(err) => {
                let text = err.to_string();
                last_error = Some(text.clone());
                if attempt < max_retries && !text.contains("usage limit") {
                    let delay = BASE_DELAY_MS * 2u64.pow(attempt);
                    sleep_ms(delay).await;
                    attempt += 1;
                    continue;
                }
                break;
            }
        }
    }

    let response = response.ok_or_else(|| last_error.unwrap_or_else(|| "Failed after retries".to_string()))?;

    push(AssistantMessageEvent::Start { partial: new_output(model) });
    let events = read_codex_sse_events(response).await?;
    let normalized = map_codex_events(&events, &mut output, options.service_tier.as_deref())?;
    let proc_options = ProcessResponsesOptions { service_tier: options.service_tier.clone() };
    process_responses_stream(&normalized, &mut output, push, model, &proc_options)
        .map_err(|e| e.to_string())?;

    // assertSuccessfulOutput: pending / error / aborted are stream failures.
    if output.stop_reason() == Some(StopReason::Pending) {
        return Err("Codex stream ended without a stop reason".to_string());
    }
    if output.stop_reason() == Some(StopReason::Error) || output.stop_reason() == Some(StopReason::Aborted) {
        let known = output.error_message().unwrap_or("").to_string();
        return Err(if known.is_empty() { "An unknown error occurred".to_string() } else { known });
    }
    Ok(output)
}

/// Stream a request against the Codex Responses SSE endpoint (upstream
/// `stream`).
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &OpenAICodexResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let Some(sender) = stream.sender() else { return stream };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let Some(api_key) = api_key.filter(|k| !k.is_empty()).map(|s| s.to_string()) else {
        return crate::event_stream::create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            format!("No API key for provider: {}", model.provider),
        );
    };

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let result = run_stream(&model, &context, client, &api_key, &options, &mut |event| {
            pusher.push(event);
        })
        .await;
        match result {
            Ok(output) => {
                let reason = match output.stop_reason().unwrap_or(StopReason::Stop) {
                    StopReason::Stop => DoneReason::Stop,
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                pusher.push(AssistantMessageEvent::Done { reason, message: output.clone() });
                pusher.end(Some(output));
            }
            Err(error_message) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_output_error_message(&mut message, error_message);
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

/// Simple (provider-neutral) `streamSimple` — resolves the reasoning effort
/// through the model's supported thinking levels and forwards (upstream
/// `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let reasoning_effort = options.reasoning.and_then(|r| {
        let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(r));
        if clamped == ModelThinkingLevel::Off {
            None
        } else {
            Some(clamped.as_str().to_string())
        }
    });
    let go = OpenAICodexResponsesOptions {
        base: options.base.clone(),
        reasoning_effort,
        reasoning_summary: None,
        service_tier: None,
        text_verbosity: None,
        tool_choice: options.tool_choice.map(|t| match t {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
        }),
    };
    stream(model, context, client, api_key, &go)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, UserContent};

    fn codex_model(id: &str) -> Model {
        let models = crate::providers::catalog_models("openai-codex");
        models.into_iter().find(|m| m.id == id).unwrap_or_else(|| {
            let mut m = Model::new(id, id, "openai-codex-responses", "openai-codex");
            m.base_url = "https://chatgpt.com/backend-api".to_string();
            m.reasoning = true;
            m.input = vec![crate::model::ModelInput::Text];
            m
        })
    }

    fn codex_ctx() -> Context {
        Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![Message::User(UserContent::string("Say hello", 1))],
            ..Default::default()
        }
    }

    fn mock_token(account_id: &str) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::STANDARD.encode(format!(
            "{{\"{JWT_CLAIM_PATH}\": {{\"chatgpt_account_id\": \"{account_id}\"}}}}"
        ));
        format!("aaa.{payload}.bbb")
    }

    /// Build the codex SSE fixture used by the upstream stream tests.
    fn codex_sse(status: &str, end_turn: Option<bool>) -> String {
        let terminal_type = if status == "incomplete" { "response.incomplete" } else { "response.completed" };
        let mut events = vec![
            format!(r#"data: {{"type":"response.output_item.added","item":{{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}}}"#),
            format!(r#"data: {{"type":"response.content_part.added","part":{{"type":"output_text","text":""}}}}"#),
            format!(r#"data: {{"type":"response.output_text.delta","delta":"Hello"}}"#),
            format!(r#"data: {{"type":"response.output_item.done","item":{{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{{"type":"output_text","text":"Hello"}}]}}}}"#),
        ];
        let end_turn_json = end_turn.map(|v| format!(",\"end_turn\":{v}")).unwrap_or_default();
        let incomplete = if status == "incomplete" { r#","incomplete_details":{"reason":"max_output_tokens"}"# } else { "" };
        events.push(format!(
            r#"data: {{"type":"{terminal_type}","response":{{"status":"{status}"{end_turn_json}{incomplete},"usage":{{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{{"cached_tokens":0}}}}}}}}"#
        ));
        format!("{}\n\n", events.join("\n\n"))
    }

    fn parse_sse(text: &str) -> Vec<SseEvent> {
        SseParser::parse_text(text)
    }

    // ------------------------------------------------------------------
    // URL / auth / headers
    // ------------------------------------------------------------------

    #[test]
    fn resolves_codex_urls() {
        assert_eq!(resolve_codex_url(None), "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(resolve_codex_url(Some("https://chatgpt.com/backend-api")), "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(resolve_codex_url(Some("https://chatgpt.com/backend-api/")), "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(resolve_codex_url(Some("https://example.com/codex")), "https://example.com/codex/responses");
        assert_eq!(resolve_codex_url(Some("https://example.com/codex/responses")), "https://example.com/codex/responses");
    }

    #[test]
    fn extracts_account_id_from_jwt() {
        let token = mock_token("acc_test");
        assert_eq!(extract_account_id(&token).unwrap(), "acc_test");
        assert!(extract_account_id("not-a-jwt").is_err());
        assert!(extract_account_id("aaa.!!!.bbb").is_err());
        let no_claim = format!(
            "aaa.{}.bbb",
            {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(r#"{"sub":"x"}"#)
            }
        );
        assert!(extract_account_id(&no_claim).is_err());
    }

    #[test]
    fn builds_sse_headers_with_session_affinity() {
        let token = mock_token("acc_test");
        let headers = build_codex_headers(None, None, "acc_test", &token, None);
        assert_eq!(headers.get("authorization").unwrap(), &format!("Bearer {token}"));
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acc_test");
        assert_eq!(headers.get("originator").unwrap(), "pi");
        assert_eq!(headers.get("openai-beta").unwrap(), "responses=experimental");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert!(!headers.contains_key("session-id"));
        assert!(!headers.contains_key("x-client-request-id"));

        let headers = build_codex_headers(None, None, "acc_test", &token, Some("sess"));
        assert_eq!(headers.get("session-id").unwrap(), "sess");
        assert_eq!(headers.get("x-client-request-id").unwrap(), "sess");

        // Session id is clamped to 64 chars before the header builder runs.
        let clamped = clamp_openai_prompt_cache_key(Some(&"x".repeat(67))).unwrap();
        let headers = build_codex_headers(None, None, "acc_test", &token, Some(&clamped));
        assert_eq!(headers.get("session-id").unwrap().len(), 64);

        // Model headers are applied first; request header null deletes.
        let mut model_headers = BTreeMap::new();
        model_headers.insert("X-Custom".to_string(), "model-value".to_string());
        let mut request_headers = crate::types::ProviderHeaders::new();
        request_headers.insert("x-custom".to_string(), None);
        request_headers.insert("X-Client-Request-Id".to_string(), Some("override".to_string()));
        let headers = build_codex_headers(Some(&model_headers), Some(&request_headers), "acc_test", &token, None);
        assert!(!headers.contains_key("x-custom"));
        assert_eq!(headers.get("x-client-request-id").unwrap(), "override");
    }

    // ------------------------------------------------------------------
    // Request body
    // ------------------------------------------------------------------

    #[test]
    fn request_body_shape_matches_upstream() {
        let model = codex_model("gpt-5.5");
        let context = codex_ctx();
        let options = OpenAICodexResponsesOptions::default();
        let body = build_request_body(&model, &context, &options, None).unwrap();
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are a helpful assistant.");
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        // System prompt is not part of `input` (includeSystemPrompt false).
        let input = body["input"].as_array().unwrap();
        assert!(!input.iter().any(|m| m.get("role") == Some(&Value::String("developer".to_string()))));
        assert_eq!(input[0]["role"], "user");
        assert!(!body.as_object().unwrap().contains_key("prompt_cache_key"));
        assert!(!body.as_object().unwrap().contains_key("reasoning"));
    }

    #[test]
    fn request_body_honors_options_and_cache_key() {
        let model = codex_model("gpt-5.5");
        let context = codex_ctx();
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                temperature: Some(0.7),
                session_id: Some("session-123".to_string()),
                ..Default::default()
            },
            service_tier: Some("priority".to_string()),
            text_verbosity: Some("high".to_string()),
            tool_choice: Some(json!("required")),
            reasoning_effort: Some("minimal".to_string()),
            reasoning_summary: Some("detailed".to_string()),
        };
        let body = build_request_body(&model, &context, &options, Some("session-123")).unwrap();
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["text"]["verbosity"], "high");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["prompt_cache_key"], "session-123");
        // minimal reasoning effort maps through the model's thinking-level map
        // to "low" (port of the upstream "clamps minimal to low" test).
        assert_eq!(body["reasoning"], json!({ "effort": "low", "summary": "detailed" }));

        // Long cache keys are clamped to 64 chars before the body is built.
        let long = clamp_openai_prompt_cache_key(Some(&"x".repeat(67))).unwrap();
        let body = build_request_body(&model, &context, &options, Some(&long)).unwrap();
        assert_eq!(body["prompt_cache_key"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn request_body_xhigh_effort_preserved() {
        let model = codex_model("gpt-5.5");
        let context = codex_ctx();
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions::default(),
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        let body = build_request_body(&model, &context, &options, None).unwrap();
        assert_eq!(body["reasoning"], json!({ "effort": "xhigh", "summary": "auto" }));
    }

    #[test]
    fn codex_tools_strict_semantics() {
        // Port of the upstream "sets Codex strict mode explicitly" test.
        let tools = vec![
            crate::types::Tool {
                name: "optional".to_string(),
                description: "Optional constrained sampling".to_string(),
                parameters: json!({ "type": "object", "properties": { "value": { "type": "string" } } }),
                constrained_sampling: None,
            },
            crate::types::Tool {
                name: "strict".to_string(),
                description: "Strict constrained sampling".to_string(),
                parameters: json!({ "type": "object", "properties": { "value": { "type": "string" } } }),
                constrained_sampling: Some(crate::types::ConstrainedSampling::JsonSchema {
                    strict: crate::types::StrictPreference::Prefer,
                }),
            },
        ];
        let out = convert_codex_tools(&tools, true, true).unwrap();
        assert_eq!(out[0]["type"], "function");
        assert_eq!(out[0]["name"], "optional");
        assert_eq!(out[0]["strict"], Value::Null);
        assert_eq!(out[1]["name"], "strict");
        assert_eq!(out[1]["strict"], true);
        // Strict schema adds additionalProperties:false.
        assert_eq!(out[1]["parameters"]["additionalProperties"], false);
    }

    #[test]
    fn reuses_shared_message_conversion_without_system_prompt() {
        let model = codex_model("gpt-5.5");
        let context = codex_ctx();
        let messages = convert_responses_messages(
            &model,
            &context,
            &CODEX_TOOL_CALL_PROVIDERS,
            &ConvertResponsesMessagesOptions { include_system_prompt: false },
        );
        assert_eq!(messages[0]["role"], "user");
    }

    // ------------------------------------------------------------------
    // Event mapping
    // ------------------------------------------------------------------

    #[test]
    fn maps_terminal_events_and_end_turn() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(&codex_sse("completed", Some(false)));
        let mut output = new_output(&model);
        let normalized = map_codex_events(&events, &mut output, None).unwrap();
        assert_eq!(normalized.len(), events.len());
        let last = normalized.last().unwrap();
        let parsed: Value = serde_json::from_str(&last.data).unwrap();
        assert_eq!(parsed["type"], "response.completed");
        assert_eq!(parsed["response"]["status"], "completed");
        let AssistantMessage::Assistant { end_turn, .. } = &output;
        let end_turn = *end_turn;
        assert_eq!(end_turn, Some(false));
    }

    #[test]
    fn maps_incomplete_to_completed_type() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(&codex_sse("incomplete", None));
        let mut output = new_output(&model);
        let normalized = map_codex_events(&events, &mut output, None).unwrap();
        let parsed: Value = serde_json::from_str(&normalized.last().unwrap().data).unwrap();
        assert_eq!(parsed["type"], "response.completed");
        assert_eq!(parsed["response"]["status"], "incomplete");
    }

    #[test]
    fn errors_on_codex_error_events() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(r#"data: {"type":"error","code":"server_error","message":"boom"}

"#);
        let mut output = new_output(&model);
        let err = map_codex_events(&events, &mut output, None).unwrap_err();
        assert_eq!(err, "Codex error: boom");
    }

    #[test]
    fn errors_on_response_failed() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(r#"data: {"type":"response.failed","response":{"error":{"code":"gone","message":"bad request"}}}

"#);
        let mut output = new_output(&model);
        let err = map_codex_events(&events, &mut output, None).unwrap_err();
        assert_eq!(err, "bad request");
    }

    #[test]
    fn normalizes_unknown_statuses_away() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(r#"data: {"type":"response.completed","response":{"status":"bogus","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#);
        let mut output = new_output(&model);
        let normalized = map_codex_events(&events, &mut output, None).unwrap();
        let parsed: Value = serde_json::from_str(&normalized[0].data).unwrap();
        assert!(parsed["response"].get("status").is_none());
    }

    #[test]
    fn resolves_service_tier_default_echo() {
        assert_eq!(resolve_codex_service_tier(Some("default"), Some("flex")), Some("flex".to_string()));
        assert_eq!(resolve_codex_service_tier(Some("default"), Some("priority")), Some("priority".to_string()));
        assert_eq!(resolve_codex_service_tier(Some("default"), None), Some("default".to_string()));
        assert_eq!(resolve_codex_service_tier(Some("priority"), Some("flex")), Some("priority".to_string()));
        assert_eq!(resolve_codex_service_tier(None, Some("flex")), Some("flex".to_string()));
        assert_eq!(resolve_codex_service_tier(None, None), None);
    }

    // ------------------------------------------------------------------
    // Full stream processing (fixture-driven, mirrors upstream stream tests)
    // ------------------------------------------------------------------

    fn process_sse_text(
        sse_text: &str,
        model: &Model,
        options: &OpenAICodexResponsesOptions,
    ) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
        let mut output = new_output(model);
        let events = parse_sse(sse_text);
        let normalized = map_codex_events(&events, &mut output, options.service_tier.as_deref()).unwrap();
        let mut pushed = Vec::new();
        let proc_options = ProcessResponsesOptions { service_tier: options.service_tier.clone() };
        process_responses_stream(&normalized, &mut output, &mut |e| pushed.push(e.clone()), model, &proc_options).unwrap();
        (output, pushed)
    }

    #[test]
    fn streams_sse_into_message_events() {
        let model = codex_model("gpt-5.5");
        let options = OpenAICodexResponsesOptions::default();
        let (message, pushed) = process_sse_text(&codex_sse("completed", None), &model, &options);
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        let text: String = message
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        let usage = message.usage().unwrap();
        assert_eq!(usage.input, 5);
        assert_eq!(usage.output, 3);
        assert_eq!(usage.total_tokens, 8);
        assert!(pushed.iter().any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
        assert!(pushed.iter().any(|e| matches!(e, AssistantMessageEvent::TextEnd { .. })));
    }

    #[test]
    fn maps_incomplete_to_length_stop() {
        let model = codex_model("gpt-5.5");
        let options = OpenAICodexResponsesOptions::default();
        let (message, _) = process_sse_text(&codex_sse("incomplete", None), &model, &options);
        assert_eq!(message.stop_reason(), Some(StopReason::Length));
        assert_eq!(message.raw_stop_reason().unwrap(), "incomplete.max_output_tokens");
    }

    #[test]
    fn service_tier_pricing_multiplier_applies_when_backend_echoes_default() {
        // Port of the upstream service-tier pricing matrix.
        for (model_id, service_tier, multiplier) in [
            ("gpt-5.5", "flex", 0.5),
            ("gpt-5.5", "priority", 2.5),
        ] {
            let mut model = codex_model(model_id);
            model.cost = crate::model::ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: None,
            };
            let sse = r#"data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}

data: {"type":"response.output_text.delta","delta":"Hello"}

data: {"type":"response.output_item.done","item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello"}]}}

data: {"type":"response.completed","response":{"status":"completed","service_tier":"default","usage":{"input_tokens":1000000,"output_tokens":1000000,"total_tokens":2000000,"input_tokens_details":{"cached_tokens":0}}}}

"#;
            let options = OpenAICodexResponsesOptions {
                service_tier: Some(service_tier.to_string()),
                ..Default::default()
            };
            let (message, _) = process_sse_text(sse, &model, &options);
            let cost = message.usage().unwrap().cost.clone();
            assert!((cost.input - 1.0 * multiplier).abs() < 1e-9, "{model_id} {service_tier}");
            assert!((cost.output - 2.0 * multiplier).abs() < 1e-9, "{model_id} {service_tier}");
            assert!((cost.total - 3.0 * multiplier).abs() < 1e-9, "{model_id} {service_tier}");
        }
    }

    // ------------------------------------------------------------------
    // Retry / error parsing
    // ------------------------------------------------------------------

    #[test]
    fn retryable_classification() {
        assert!(is_retryable_error(429, "rate limited, try later"));
        assert!(is_retryable_error(500, ""));
        assert!(is_retryable_error(503, ""));
        assert!(is_retryable_error(200, "upstream connect error"));
        assert!(!is_retryable_error(429, "GoUsageLimitError: quota"));
        assert!(!is_retryable_error(429, "insufficient_quota for plan"));
        assert!(!is_retryable_error(400, "bad request"));
    }

    #[test]
    fn parses_friendly_usage_limit_errors() {
        let (msg, friendly) = parse_error_response(
            r#"{"error":{"code":"usage_limit_reached","message":"raw message","plan_type":"free","resets_at":99999999999999}}"#,
            429,
            "Too Many Requests",
        );
        assert_eq!(msg, "raw message");
        assert!(friendly.unwrap().starts_with("You have hit your ChatGPT usage limit (free plan)."));
    }

    #[test]
    fn retry_after_delay_parsing() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after-ms".to_string(), "250".to_string());
        assert_eq!(get_retry_after_delay_ms(&headers), Some(250));
        headers.insert("retry-after".to_string(), "2".to_string());
        // retry-after-ms wins over retry-after.
        assert_eq!(get_retry_after_delay_ms(&headers), Some(250));
        let mut headers = BTreeMap::new();
        headers.insert("retry-after".to_string(), "2".to_string());
        assert_eq!(get_retry_after_delay_ms(&headers), Some(2000));
        let mut headers = BTreeMap::new();
        headers.insert("retry-after".to_string(), "Wed, 21 Oct 2030 07:28:00 GMT".to_string());
        assert_eq!(get_retry_after_delay_ms(&headers), None);
        assert_eq!(get_retry_after_delay_ms(&BTreeMap::new()), None);
    }

    // ------------------------------------------------------------------
    // Stream entry points
    // ------------------------------------------------------------------

    #[test]
    fn stream_without_key_is_terminal_error() {
        let model = codex_model("gpt-5.5");
        let s = stream(&model, &Context::default(), reqwest::Client::new(), None, &OpenAICodexResponsesOptions::default());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (events, msg) = rt.block_on(s.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(err.contains("No API key for provider: openai-codex"), "{err}");
    }

    #[test]
    fn invalid_token_is_a_terminal_error() {
        let model = codex_model("gpt-5.5");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (events, msg) = rt
            .block_on(async {
                let s = stream(&model, &Context::default(), reqwest::Client::new(), Some("not-a-jwt"), &OpenAICodexResponsesOptions::default());
                tokio::time::timeout(std::time::Duration::from_secs(5), s.collect()).await
            })
            .expect("timed out waiting for the invalid-token error stream");
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(err.contains("Failed to extract accountId from token"), "{err}");
    }

    #[test]
    fn stream_simple_resolves_xhigh_reasoning() {
        let model = codex_model("gpt-5.5");
        let options = SimpleStreamOptions {
            base: StreamOptions::default(),
            reasoning: Some(crate::types::ThinkingLevel::Xhigh),
            ..Default::default()
        };
        // The API key / network path would run an HTTP request; the reasoning
        // resolution is observable through the options it would feed into the
        // body builder.
        let effort = stream_simple_reasoning_effort_for_test(&model, &options);
        assert_eq!(effort.as_deref(), Some("xhigh"));
        // And the body maps it to the thinking-level map value.
        let body = build_request_body(
            &model,
            &codex_ctx(),
            &OpenAICodexResponsesOptions {
                base: StreamOptions::default(),
                reasoning_effort: effort,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(body["reasoning"], json!({ "effort": "xhigh", "summary": "auto" }));
    }

    fn stream_simple_reasoning_effort_for_test(
        model: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<String> {
        let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(options.reasoning.unwrap()));
        if clamped == ModelThinkingLevel::Off {
            None
        } else {
            Some(clamped.as_str().to_string())
        }
    }
}
