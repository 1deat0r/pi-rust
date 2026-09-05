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
//! - WebSocket transport is implemented (`transport: "auto"` tries the
//!   WebSocket path and falls back to SSE; "websocket" forces WS; "sse"
//!   forces SSE). Session-scoped WebSocket caching, idle/max-age eviction,
//!   cached-context delta requests, and missing-continuation recovery are
//!   implemented with the upstream session/account keying semantics.
//! - SSE request bodies use zstd compression when the native zstd runtime is
//!   available, with the same uncompressed fallback as upstream browser
//!   builds.
//! - `options.signal` is represented by the Rust runtime's cooperative
//!   `Arc<AtomicBool>` signal. It cancels connection, request, retry, and body
//!   reads and produces the upstream `aborted` terminal result.
//! - `onPayload` is an asynchronous Rust callback with the same replace-or-
//!   preserve semantics as the upstream hook and runs before transport choice.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::{atomic::Ordering, Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use futures_util::StreamExt;

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{clamp_thinking_level, Model};
use crate::sse::{SseEvent, SseParser};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason, Message,
    ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, Tool, ToolChoice, Usage,
};

use super::constrained_sampling::{
    create_grammar_tool_input_properties, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use super::mistral_conversations::pi_user_agent;
use super::openai_responses_shared::*;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_WEBSOCKET_MAX_AGE: Duration = Duration::from_secs(55 * 60);

const CODEX_RESPONSE_STATUSES: [&str; 6] = [
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];

const REQUEST_ABORTED: &str = "Request was aborted";

type CodexWsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct CachedWebSocketContinuationState {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct CachedWebSocketConnection {
    socket: Arc<tokio::sync::Mutex<CodexWsStream>>,
    busy: bool,
    created_at: Instant,
    continuation: Option<CachedWebSocketContinuationState>,
    idle_generation: u64,
}

type WebSocketSessionCache =
    HashMap<String, HashMap<String, Arc<Mutex<CachedWebSocketConnection>>>>;

static WEBSOCKET_SESSION_CACHE: LazyLock<Mutex<WebSocketSessionCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The upstream keeps a session-level circuit breaker after a WebSocket
/// transport failure. Without this state, every subsequent request in a
/// degraded session would repeat the failed WS handshake before using SSE.
/// Keep only the timestamp: session ids are not credentials and the entry is
/// naturally bounded by the WebSocket session lifetime.
static WEBSOCKET_SSE_FALLBACK_SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Observable counters for the Codex WebSocket cache. This mirrors the
/// upstream debug surface used by transport probes and deterministic tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenAICodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: usize,
    pub last_delta_input_items: Option<usize>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: bool,
    pub last_websocket_error: Option<String>,
}

static WEBSOCKET_DEBUG_STATS: LazyLock<Mutex<HashMap<String, OpenAICodexWebSocketDebugStats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_openai_codex_websocket_debug_stats(
    session_id: &str,
) -> Option<OpenAICodexWebSocketDebugStats> {
    WEBSOCKET_DEBUG_STATS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(session_id)
        .cloned()
}

/// Close cached Codex WebSocket sessions, matching upstream's session-resource
/// cleanup hook. A session id limits cleanup to one session; `None` closes all
/// cached sessions. Socket close is scheduled asynchronously because this API
/// is also used from synchronous session teardown paths.
pub fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let entries = {
        let mut cache = WEBSOCKET_SESSION_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match session_id {
            Some(session_id) => cache
                .remove(session_id)
                .into_iter()
                .flat_map(|accounts| accounts.into_values())
                .collect::<Vec<_>>(),
            None => cache
                .drain()
                .flat_map(|(_, accounts)| accounts.into_values())
                .collect::<Vec<_>>(),
        }
    };
    for entry in entries {
        let socket = entry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .socket
            .clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { close_websocket(&socket).await });
        }
    }
}

/// Reset the session-scoped WebSocket fallback circuit. This mirrors the
/// upstream debug/reset boundary and is useful after credentials or transport
/// configuration change. It does not close healthy cached sockets.
pub fn reset_openai_codex_websocket_debug_stats(session_id: Option<&str>) {
    let mut sessions = WEBSOCKET_SSE_FALLBACK_SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut stats = WEBSOCKET_DEBUG_STATS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(session_id) = session_id {
        sessions.remove(session_id);
        stats.remove(session_id);
    } else {
        sessions.clear();
        stats.clear();
    }
}

/// Backwards-compatible narrow reset name retained for callers that only care
/// about the fallback circuit.
pub fn reset_openai_codex_websocket_fallback(session_id: Option<&str>) {
    reset_openai_codex_websocket_debug_stats(session_id);
}

fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    let Some(session_id) = session_id else {
        return false;
    };
    let mut sessions = WEBSOCKET_SSE_FALLBACK_SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    match sessions.get(session_id).copied() {
        Some(recorded_at) if recorded_at.elapsed() < SESSION_WEBSOCKET_MAX_AGE => true,
        Some(_) => {
            sessions.remove(session_id);
            false
        }
        None => false,
    }
}

fn record_websocket_sse_fallback(session_id: Option<&str>) {
    if let Some(session_id) = session_id {
        let fallback_active = WEBSOCKET_SSE_FALLBACK_SESSIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(session_id);
        let mut stats = WEBSOCKET_DEBUG_STATS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let stats = stats.entry(session_id.to_string()).or_default();
        stats.sse_fallbacks += 1;
        stats.websocket_fallback_active = fallback_active;
    }
}

fn record_websocket_failure(session_id: Option<&str>, error: &str) {
    let Some(session_id) = session_id else {
        return;
    };
    WEBSOCKET_SSE_FALLBACK_SESSIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(session_id.to_string(), Instant::now());
    let mut stats = WEBSOCKET_DEBUG_STATS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let stats = stats.entry(session_id.to_string()).or_default();
    stats.websocket_failures += 1;
    stats.websocket_fallback_active = true;
    stats.last_websocket_error = Some(error.to_string());
}

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
    /// Transport selection (upstream `transport`): "auto" (default) tries
    /// the WebSocket path first and falls back to SSE; "sse" forces SSE;
    /// "websocket" forces the WebSocket path.
    pub transport: Option<String>,
}

fn effective_transport(options: &OpenAICodexResponsesOptions) -> &str {
    options
        .transport
        .as_deref()
        .or(options.base.transport.as_deref())
        .unwrap_or(crate::types::TRANSPORT_AUTO)
}

fn is_previous_response_not_found_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("previous_response_not_found")
        || (normalized.contains("previous response") && normalized.contains("not found"))
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

/// Resolve the Codex WebSocket endpoint from a base URL (upstream
/// `resolveCodexWebSocketUrl`): https -> wss, http -> ws.
fn resolve_codex_websocket_url(base_url: Option<&str>) -> String {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url
    }
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
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(parts[1]))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
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
    headers.insert(
        "openai-beta".to_string(),
        "responses=experimental".to_string(),
    );
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
/// while constrained JSON-schema and OpenAI grammar tools use the shared
/// resolver.
fn convert_codex_tools(
    tools: &[Tool],
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
) -> Result<Vec<Value>, String> {
    let mut result: Vec<Value> = Vec::new();
    for tool in tools {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?
        {
            result.push(json!({
                "type": "custom",
                "name": tool.name,
                "description": tool.description,
                "format": {
                    "type": "grammar",
                    "syntax": grammar.format,
                    "definition": grammar.definition,
                },
            }));
            continue;
        }
        let constrained_strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
        let parameters = get_json_schema_tool_parameters(tool, constrained_strict)?;
        let mut function_tool = json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": parameters,
        });
        if supports_strict_mode {
            function_tool["strict"] = constrained_strict.map(Value::Bool).unwrap_or(Value::Null);
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
    let supports_strict_mode = compat
        .and_then(|c| c.get("supportsStrictMode"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let supports_openai_grammar_tools = compat
        .and_then(|c| c.get("supportsOpenAIGrammarTools"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let deferred_tools_mode = if compat
        .and_then(|c| c.get("supportsAdditionalTools"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        Some("additional-tools".to_string())
    } else if compat
        .and_then(|c| c.get("supportsToolSearch"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        Some("tool-search".to_string())
    } else {
        None
    };
    let (immediate_tools, deferred_tools) =
        split_deferred_tools(context, deferred_tools_mode.is_some());
    let tool_options = ConvertResponsesToolsOptions {
        strict: None,
        supports_strict_mode,
        supports_openai_grammar_tools,
    };

    let grammar_properties =
        create_grammar_tool_input_properties(&context.tools, supports_openai_grammar_tools)?;
    let messages = convert_responses_messages_checked(
        model,
        context,
        &CODEX_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            include_system_prompt: false,
            grammar_tool_input_properties: grammar_properties,
            deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
            deferred_tools_mode,
            deferred_tools_strict_null: true,
            tool_options: Some(tool_options),
        },
    )?;

    let instructions = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or("You are a helpful assistant.");
    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": instructions,
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
    if !immediate_tools.is_empty() {
        body["tools"] = json!(convert_codex_tools(
            &immediate_tools,
            supports_strict_mode,
            supports_openai_grammar_tools
        )?);
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

fn create_codex_grammar_properties(
    model: &Model,
    context: &Context,
) -> Result<BTreeMap<String, String>, String> {
    let supports = model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("supportsOpenAIGrammarTools"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    create_grammar_tool_input_properties(&context.tools, supports)
}

// ---------------------------------------------------------------------------
// Retry helpers
// ---------------------------------------------------------------------------

#[allow(clippy::panic)] // compile-time literal regex; failure is a build defect
fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    regex::RegexBuilder::new(
        r"GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing",
    )
    .case_insensitive(true)
    .build()
    .unwrap_or_else(|error| panic!("terminal rate-limit regex must compile: {error}"))
    .is_match(error_text)
}

#[allow(clippy::panic)] // compile-time literal regex; failure is a build defect
fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    regex::RegexBuilder::new(
        r"rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused",
    )
    .case_insensitive(true)
    .build()
    .unwrap_or_else(|error| panic!("retryable regex must compile: {error}"))
    .is_match(error_text)
}

/// Read retry-after guidance from response headers (upstream
/// `getRetryAfterDelayMs`). Supports millisecond, seconds, and standard
/// HTTP-date forms.
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
    parse_http_date_delay_ms(retry_after)
}

fn parse_http_date_delay_ms(value: &str) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let _weekday = fields.next()?;
    let day = fields.next()?.parse::<i64>().ok()?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fields.next()?.parse::<i64>().ok()?;
    let mut time = fields.next()?.split(':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let second = time.next()?.parse::<i64>().ok()?;
    if time.next().is_some() || fields.next()? != "GMT" {
        return None;
    }
    if !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
        || !(1..=12).contains(&month)
    {
        return None;
    }

    // Proleptic Gregorian calendar, days relative to 1970-01-01. This is
    // the same calendar used by Date.parse for RFC 7231 HTTP-date values.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let target_ms = days
        .checked_mul(86_400_000)?
        .checked_add((hour * 3_600 + minute * 60 + second) * 1_000)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i128;
    Some((i128::from(target_ms) - now_ms).max(0) as u64)
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
    let max_retry_delay_ms = base
        .base
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_retry_delay_ms > 0 && delay_ms > max_retry_delay_ms {
        return Err(retry_delay_exceeded_message(delay_ms, max_retry_delay_ms));
    }
    Ok(delay_ms)
}

/// Format a reqwest transport error with its full source chain.
fn format_transport_error(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(s) = source {
        text.push_str(&format!(": {s}"));
        source = s.source();
    }
    text
}

fn is_aborted_signal(signal: Option<&crate::types::AbortSignal>) -> bool {
    signal.is_some_and(|signal| signal.load(Ordering::SeqCst))
}

async fn wait_for_abort(signal: crate::types::AbortSignal) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn execute_codex_request(
    client: &reqwest::Client,
    request: reqwest::Request,
    timeout_ms: Option<u64>,
) -> Result<reqwest::Response, String> {
    let execute = client.execute(request);
    match timeout_ms.filter(|timeout| *timeout > 0) {
        Some(timeout) => {
            match tokio::time::timeout(Duration::from_millis(timeout), execute).await {
                Ok(result) => result.map_err(|error| format_transport_error(&error)),
                Err(_) => Err(format!(
                    "Codex SSE response headers timed out after {timeout}ms"
                )),
            }
        }
        None => execute
            .await
            .map_err(|error| format_transport_error(&error)),
    }
}

async fn read_codex_response_text(
    response: reqwest::Response,
    signal: Option<crate::types::AbortSignal>,
) -> Result<String, String> {
    if let Some(signal) = signal {
        if signal.load(Ordering::SeqCst) {
            return Err(REQUEST_ABORTED.to_string());
        }
        tokio::select! {
            result = response.text() => result.map_err(|error| format_transport_error(&error)),
            _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
        }
    } else {
        response
            .text()
            .await
            .map_err(|error| format_transport_error(&error))
    }
}

async fn sleep_ms(ms: u64, signal: Option<crate::types::AbortSignal>) -> Result<(), String> {
    if is_aborted_signal(signal.as_ref()) {
        return Err(REQUEST_ABORTED.to_string());
    }
    if let Some(signal) = signal {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {
                if signal.load(Ordering::SeqCst) {
                    Err(REQUEST_ABORTED.to_string())
                } else {
                    Ok(())
                }
            }
            _ = wait_for_abort(signal.clone()) => Err(REQUEST_ABORTED.to_string()),
        }
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok(())
    }
}

async fn apply_payload_hook(
    body: Value,
    model: &Model,
    hook: Option<&crate::types::OnPayloadFn>,
) -> Value {
    let Some(hook) = hook else { return body };
    hook(body.clone(), model.clone()).await.unwrap_or(body)
}

// The pinned upstream uses node:zlib's zstdCompressSync when that native
// runtime is present. The shipped Linux binary has the same native zstd
// facility available through libzstd; keep the call isolated so targets that
// do not provide it retain the upstream uncompressed fallback.
#[cfg(target_os = "linux")]
#[link(name = "zstd")]
unsafe extern "C" {
    fn ZSTD_compressBound(src_size: usize) -> usize;
    fn ZSTD_compress(
        dst: *mut u8,
        dst_capacity: usize,
        src: *const u8,
        src_size: usize,
        compression_level: i32,
    ) -> usize;
    fn ZSTD_isError(code: usize) -> u32;
}

#[cfg(target_os = "linux")]
fn compress_request_body_zstd(body_json: &[u8]) -> Option<Vec<u8>> {
    const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
    // SAFETY: the libzstd functions only read the immutable source slice and
    // write at most the capacity returned by ZSTD_compressBound.
    unsafe {
        let bound = ZSTD_compressBound(body_json.len());
        if bound == 0 {
            return None;
        }
        let mut compressed = vec![0_u8; bound];
        let written = ZSTD_compress(
            compressed.as_mut_ptr(),
            compressed.len(),
            body_json.as_ptr(),
            body_json.len(),
            REQUEST_COMPRESSION_ZSTD_LEVEL,
        );
        if ZSTD_isError(written) != 0 {
            return None;
        }
        compressed.truncate(written);
        Some(compressed)
    }
}

#[cfg(not(target_os = "linux"))]
fn compress_request_body_zstd(_body_json: &[u8]) -> Option<Vec<u8>> {
    None
}

// ---------------------------------------------------------------------------
// Error response parsing
// ---------------------------------------------------------------------------

/// Parse a Codex error response into `(message, friendlyMessage)` (upstream
/// `parseErrorResponse`).
#[allow(clippy::panic)] // compile-time literal regex; failure is a build defect
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
            let usage_limit = regex::RegexBuilder::new(
                r"usage_limit_reached|usage_not_included|rate_limit_exceeded",
            )
            .case_insensitive(true)
            .build()
            .unwrap_or_else(|error| panic!("usage-limit regex must compile: {error}"))
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
                let when = mins
                    .map(|m| format!(" Try again in ~{m} min."))
                    .unwrap_or_default();
                friendly_message = Some(
                    format!("You have hit your ChatGPT usage limit{plan}.{when}")
                        .trim()
                        .to_string(),
                );
            }
            if let Some(err_message) = err
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|m| !m.is_empty())
            {
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
        .or_else(|| {
            nested
                .and_then(|n| n.get("message"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string());
    (code, message)
}

/// Resolve the effective service tier when the backend echoes `default`
/// (upstream `resolveCodexServiceTier`).
fn resolve_codex_service_tier(
    response_tier: Option<&str>,
    request_tier: Option<&str>,
) -> Option<String> {
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
                return Err(message
                    .or(code)
                    .unwrap_or("Codex response failed")
                    .to_string());
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
                    if let Some(tier) =
                        resolve_codex_service_tier(response_tier, request_service_tier)
                    {
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

fn process_codex_event(
    state: &mut ProcessResponsesStreamState,
    event: &SseEvent,
    output: &mut AssistantMessage,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
    model: &Model,
    options: &ProcessResponsesOptions,
    request_service_tier: Option<&str>,
) -> Result<(), String> {
    if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
        return Ok(());
    }
    let normalized = map_codex_events(std::slice::from_ref(event), output, request_service_tier)?;
    process_responses_stream_chunk(state, &normalized, output, push, model, options)
}

/// Consume a Codex SSE response incrementally.  The upstream parser is an
/// async generator: each framed event is mapped and emitted before the next
/// network chunk is read, and the reader is abandoned at the terminal event.
async fn process_codex_sse_stream(
    response: reqwest::Response,
    output: &mut AssistantMessage,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
    model: &Model,
    options: &ProcessResponsesOptions,
    request_service_tier: Option<&str>,
    signal: Option<crate::types::AbortSignal>,
) -> Result<(), String> {
    let mut parser = SseParser::new();
    let mut state = ProcessResponsesStreamState::default();
    let mut stream = response.bytes_stream();

    loop {
        if is_aborted_signal(signal.as_ref()) {
            return Err(REQUEST_ABORTED.to_string());
        }
        let next_chunk = if let Some(signal) = signal.clone() {
            tokio::select! {
                chunk = stream.next() => chunk,
                _ = wait_for_abort(signal) => return Err(REQUEST_ABORTED.to_string()),
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = next_chunk else { break };
        let chunk = chunk.map_err(|error| format!("Codex SSE read failed: {error}"))?;
        for event in parser.push_bytes(&chunk) {
            process_codex_event(
                &mut state,
                &event,
                output,
                push,
                model,
                options,
                request_service_tier,
            )?;
            if state.saw_terminal_response_event() {
                return Ok(());
            }
        }
    }

    for event in parser.finish() {
        process_codex_event(
            &mut state,
            &event,
            output,
            push,
            model,
            options,
            request_service_tier,
        )?;
        if state.saw_terminal_response_event() {
            return Ok(());
        }
    }
    if state.saw_terminal_response_event() {
        Ok(())
    } else {
        Err("OpenAI Responses stream ended before a terminal response event".to_string())
    }
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

fn redact_codex_error(detail: &str, secrets: &[&str]) -> String {
    let mut detail = detail.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            detail = detail.replace(secret, "<redacted>");
        }
    }
    if detail.chars().count() > 512 {
        let end = detail
            .char_indices()
            .nth(512)
            .map(|(index, _)| index)
            .unwrap_or(detail.len());
        detail.truncate(end);
        detail.push('…');
    }
    detail
}

fn request_body_without_input(body: &Value) -> Value {
    let mut copy = body.clone();
    if let Value::Object(map) = &mut copy {
        map.remove("input");
        map.remove("previous_response_id");
    }
    copy
}

fn body_input(body: &Value) -> Vec<Value> {
    body.get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn websocket_session_expired(created_at: Instant, now: Instant) -> bool {
    now.checked_duration_since(created_at)
        .is_some_and(|age| age >= SESSION_WEBSOCKET_MAX_AGE)
}

fn should_evict_idle_websocket(busy: bool, generation: u64, expected_generation: u64) -> bool {
    !busy && generation == expected_generation
}

fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &CachedWebSocketContinuationState,
) -> Option<Vec<Value>> {
    if request_body_without_input(body)
        != request_body_without_input(&continuation.last_request_body)
    {
        return None;
    }
    let current = body_input(body);
    let mut baseline = body_input(&continuation.last_request_body);
    baseline.extend(continuation.last_response_items.clone());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline[..] {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

fn build_cached_websocket_request_body(
    entry: &mut CachedWebSocketConnection,
    body: &Value,
) -> Value {
    let Some(continuation) = entry.continuation.clone() else {
        return body.clone();
    };
    let Some(delta) = get_cached_websocket_input_delta(body, &continuation) else {
        entry.continuation = None;
        return body.clone();
    };
    if continuation.last_response_id.is_empty() {
        entry.continuation = None;
        return body.clone();
    }
    let mut out = body.clone();
    if let Value::Object(map) = &mut out {
        map.insert(
            "previous_response_id".to_string(),
            Value::String(continuation.last_response_id),
        );
        map.insert("input".to_string(), Value::Array(delta));
    }
    out
}

async fn connect_codex_websocket(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<CodexWsStream, String> {
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
            .map_err(|e| format!("Failed to build WebSocket request: {e}"))?;
    let request_headers = request.headers_mut();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            request_headers.insert(name, value);
        }
    }
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?;
    Ok(ws)
}

async fn next_codex_websocket_message(
    ws: &mut CodexWsStream,
    signal: Option<crate::types::AbortSignal>,
    idle_timeout_ms: Option<u64>,
) -> Result<
    Option<Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>>,
    String,
> {
    let idle = idle_timeout_ms.filter(|timeout| *timeout > 0);
    match (signal, idle) {
        (Some(signal), Some(timeout)) => {
            tokio::select! {
                message = ws.next() => Ok(message),
                _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
                _ = tokio::time::sleep(Duration::from_millis(timeout)) => {
                    Err(format!("WebSocket idle timeout after {timeout}ms"))
                }
            }
        }
        (Some(signal), None) => {
            tokio::select! {
                message = ws.next() => Ok(message),
                _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
            }
        }
        (None, Some(timeout)) => {
            match tokio::time::timeout(Duration::from_millis(timeout), ws.next()).await {
                Ok(message) => Ok(message),
                Err(_) => Err(format!("WebSocket idle timeout after {timeout}ms")),
            }
        }
        (None, None) => Ok(ws.next().await),
    }
}

struct AcquiredWebSocket {
    socket: Arc<tokio::sync::Mutex<CodexWsStream>>,
    entry: Option<Arc<Mutex<CachedWebSocketConnection>>>,
    reused: bool,
}

async fn acquire_websocket(
    url: &str,
    headers: &BTreeMap<String, String>,
    session_id: Option<&str>,
    account_id: &str,
) -> Result<AcquiredWebSocket, String> {
    let Some(session_id) = session_id else {
        let socket = Arc::new(tokio::sync::Mutex::new(
            connect_codex_websocket(url, headers).await?,
        ));
        return Ok(AcquiredWebSocket {
            socket,
            entry: None,
            reused: false,
        });
    };

    let mut cached_entry = None;
    let mut busy_cached_entry = false;
    let mut expired_entry = None;
    {
        let mut cache = WEBSOCKET_SESSION_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(accounts) = cache.get_mut(session_id) {
            if let Some(entry) = accounts.get(account_id).cloned() {
                let mut guard = entry.lock().unwrap_or_else(|error| error.into_inner());
                if !guard.busy && websocket_session_expired(guard.created_at, Instant::now()) {
                    drop(guard);
                    accounts.remove(account_id);
                    expired_entry = Some(entry);
                } else if guard.busy {
                    busy_cached_entry = true;
                } else if !guard.busy {
                    guard.busy = true;
                    guard.idle_generation = guard.idle_generation.wrapping_add(1);
                    cached_entry = Some(entry.clone());
                }
            }
            if accounts.is_empty() {
                cache.remove(session_id);
            }
        }
    }
    if let Some(entry) = expired_entry {
        let socket = entry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .socket
            .clone();
        close_websocket(&socket).await;
    }
    if let Some(entry) = cached_entry {
        let socket = entry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .socket
            .clone();
        return Ok(AcquiredWebSocket {
            socket,
            entry: Some(entry),
            reused: true,
        });
    }

    let socket = Arc::new(tokio::sync::Mutex::new(
        connect_codex_websocket(url, headers).await?,
    ));
    if busy_cached_entry {
        return Ok(AcquiredWebSocket {
            socket,
            entry: None,
            reused: false,
        });
    }
    let entry = Arc::new(Mutex::new(CachedWebSocketConnection {
        socket: socket.clone(),
        busy: true,
        created_at: Instant::now(),
        continuation: None,
        idle_generation: 0,
    }));
    let inserted = {
        let mut cache = WEBSOCKET_SESSION_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let accounts = cache.entry(session_id.to_string()).or_default();
        if accounts.contains_key(account_id) {
            false
        } else {
            accounts.insert(account_id.to_string(), entry.clone());
            true
        }
    };
    Ok(AcquiredWebSocket {
        socket,
        entry: inserted.then_some(entry),
        reused: false,
    })
}

async fn close_websocket(socket: &Arc<tokio::sync::Mutex<CodexWsStream>>) {
    let mut ws = socket.lock().await;
    let _ = ws.close(None).await;
}

async fn close_uncached_websocket(acquired: &AcquiredWebSocket) {
    if acquired.entry.is_none() {
        close_websocket(&acquired.socket).await;
    }
}

fn release_websocket(
    session_id: Option<&str>,
    account_id: &str,
    entry: Option<Arc<Mutex<CachedWebSocketConnection>>>,
    keep: bool,
) {
    let Some(entry) = entry else {
        return;
    };
    let Some(session_id) = session_id.map(str::to_string) else {
        return;
    };
    let socket = entry
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .socket
        .clone();
    if !keep {
        let removed = {
            let mut cache = WEBSOCKET_SESSION_CACHE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut removed = false;
            if let Some(accounts) = cache.get_mut(&session_id) {
                if accounts
                    .get(account_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    accounts.remove(account_id);
                    removed = true;
                }
                if accounts.is_empty() {
                    cache.remove(&session_id);
                }
            }
            removed
        };
        if removed {
            tokio::spawn(async move { close_websocket(&socket).await });
        }
        return;
    }
    let idle_generation = {
        let mut guard = entry.lock().unwrap_or_else(|error| error.into_inner());
        guard.busy = false;
        guard.idle_generation = guard.idle_generation.wrapping_add(1);
        guard.idle_generation
    };
    let account_id = account_id.to_string();
    let entry_for_timer = entry.clone();
    tokio::spawn(async move {
        tokio::time::sleep(SESSION_WEBSOCKET_CACHE_TTL).await;
        let remove = {
            let mut cache = WEBSOCKET_SESSION_CACHE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut remove = false;
            if let Some(accounts) = cache.get_mut(&session_id) {
                if accounts
                    .get(&account_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry_for_timer))
                {
                    let guard = entry_for_timer
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    remove = should_evict_idle_websocket(
                        guard.busy,
                        guard.idle_generation,
                        idle_generation,
                    );
                    if remove {
                        accounts.remove(&account_id);
                    }
                }
                if accounts.is_empty() {
                    cache.remove(&session_id);
                }
            }
            remove
        };
        if remove {
            close_websocket(&socket).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Main stream functions
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WebSocketStreamState {
    started: bool,
    non_transport_error: bool,
    api_error_code: Option<String>,
}

/// WebSocket transport (upstream `processWebSocketStream`): connect, send
/// `{ type: "response.create", ...body }`, read JSON frames until a terminal
/// event, and feed them through the shared responses processing. Returns the
/// final assistant message.
async fn run_stream_ws_with_state(
    model: &Model,
    context: &Context,
    api_key: &str,
    options: &OpenAICodexResponsesOptions,
    body: &Value,
    state: &mut WebSocketStreamState,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
) -> Result<AssistantMessage, String> {
    let mut output = new_output(model);
    let transport = effective_transport(options);
    let account_id = extract_account_id(api_key)?;
    let cache_session_id =
        if options.base.cache_retention.as_deref() == Some(crate::types::CACHE_RETENTION_NONE) {
            None
        } else {
            clamp_openai_prompt_cache_key(options.base.session_id.as_deref())
        };
    let grammar_tool_input_properties = create_codex_grammar_properties(model, context)?;
    let request_id = cache_session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // WebSocket headers (upstream `buildWebSocketHeaders`): base headers minus
    // accept/content-type, with the responses-websockets beta and request id.
    let mut headers = build_codex_headers(
        model.headers.as_ref(),
        options.base.base.headers.as_ref(),
        &account_id,
        api_key,
        None,
    );
    headers.remove("accept");
    headers.remove("content-type");
    headers.insert(
        "openai-beta".to_string(),
        "responses_websockets=2026-02-06".to_string(),
    );
    headers.insert("x-client-request-id".to_string(), request_id.clone());
    headers.insert("session-id".to_string(), request_id.clone());

    let url = resolve_codex_websocket_url(Some(&model.base_url));
    let connect_timeout_ms = options
        .base
        .websocket_connect_timeout_ms
        .unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);

    let acquire_session_id = cache_session_id.as_deref();
    let connect = acquire_websocket(&url, &headers, acquire_session_id, &account_id);

    let acquired_result = if connect_timeout_ms > 0 {
        {
            let timeout = connect_timeout_ms;
            let connect = tokio::time::timeout(Duration::from_millis(timeout), connect);
            if let Some(signal) = options.base.abort_signal.clone() {
                tokio::select! {
                    result = connect => match result {
                        Ok(result) => result,
                        Err(_) => Err(format!("WebSocket connect timed out after {timeout}ms")),
                    },
                    _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
                }
            } else {
                match connect.await {
                    Ok(result) => result,
                    Err(_) => Err(format!("WebSocket connect timed out after {timeout}ms")),
                }
            }
        }
    } else {
        if let Some(signal) = options.base.abort_signal.clone() {
            tokio::select! {
                result = connect => result,
                _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
            }
        } else {
            connect.await
        }
    };
    let acquired = acquired_result?;

    let use_cached_context = matches!(
        transport,
        crate::types::TRANSPORT_WEBSOCKET_CACHED | crate::types::TRANSPORT_AUTO
    );
    let request_body = if use_cached_context {
        if let Some(entry) = &acquired.entry {
            build_cached_websocket_request_body(
                &mut entry.lock().unwrap_or_else(|error| error.into_inner()),
                body,
            )
        } else {
            body.clone()
        }
    } else {
        body.clone()
    };

    if let Some(session_id) = cache_session_id.as_deref() {
        let mut all_stats = WEBSOCKET_DEBUG_STATS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let stats = all_stats.entry(session_id.to_string()).or_default();
        stats.requests += 1;
        if acquired.reused {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if use_cached_context {
            stats.cached_context_requests += 1;
        }
        if request_body.get("store").and_then(Value::as_bool) == Some(true) {
            stats.store_true_requests += 1;
        }
        let input_items = request_body
            .get("input")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        stats.last_input_items = input_items;
        if let Some(previous_response_id) = request_body
            .get("previous_response_id")
            .and_then(Value::as_str)
        {
            stats.delta_requests += 1;
            stats.last_delta_input_items = Some(input_items);
            stats.last_previous_response_id = Some(previous_response_id.to_string());
        } else {
            stats.full_context_requests += 1;
            stats.last_delta_input_items = None;
            stats.last_previous_response_id = None;
        }
    }

    // Send the request frame.
    let mut frame = serde_json::json!({ "type": "response.create" });
    if let serde_json::Value::Object(map) = &mut frame {
        if let serde_json::Value::Object(body_map) = request_body {
            for (k, v) in body_map {
                map.insert(k, v);
            }
        }
    }
    use futures_util::SinkExt as _;
    let mut ws = acquired.socket.lock().await;
    let send_result = if let Some(signal) = options.base.abort_signal.clone() {
        tokio::select! {
            result = ws.send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string())) => result,
            _ = wait_for_abort(signal) => Err(tokio_tungstenite::tungstenite::Error::Io(
                std::io::Error::new(std::io::ErrorKind::Interrupted, REQUEST_ABORTED),
            )),
        }
    } else {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            frame.to_string(),
        ))
        .await
    };
    if let Err(e) = send_result {
        drop(ws);
        close_uncached_websocket(&acquired).await;
        release_websocket(
            acquire_session_id,
            &account_id,
            acquired.entry.clone(),
            false,
        );
        return Err(format!("WebSocket send failed: {e}"));
    }

    // Read and process frames until a terminal event. The upstream WebSocket
    // adapter is an async generator: deltas must reach the caller before the
    // provider sends the terminal response, rather than being buffered until
    // the socket has finished.
    let mut saw_completion = false;
    let mut stream_state = ProcessResponsesStreamState::default();
    let proc_options = ProcessResponsesOptions {
        service_tier: options.service_tier.clone(),
        grammar_tool_input_properties: grammar_tool_input_properties.clone(),
    };
    loop {
        let message = match next_codex_websocket_message(
            &mut ws,
            options.base.abort_signal.clone(),
            options.base.base.timeout_ms,
        )
        .await
        {
            Err(error) => {
                drop(ws);
                close_uncached_websocket(&acquired).await;
                release_websocket(
                    acquire_session_id,
                    &account_id,
                    acquired.entry.clone(),
                    false,
                );
                return Err(error);
            }
            Ok(message) => match message {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    drop(ws);
                    close_uncached_websocket(&acquired).await;
                    release_websocket(
                        acquire_session_id,
                        &account_id,
                        acquired.entry.clone(),
                        false,
                    );
                    return Err(format!("WebSocket read failed: {error}"));
                }
                None => {
                    drop(ws);
                    close_uncached_websocket(&acquired).await;
                    release_websocket(
                        acquire_session_id,
                        &account_id,
                        acquired.entry.clone(),
                        false,
                    );
                    return Err("WebSocket closed before a terminal event".to_string());
                }
            },
        };
        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                String::from_utf8_lossy(&b).to_string()
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                if saw_completion {
                    break;
                }
                drop(ws);
                close_uncached_websocket(&acquired).await;
                release_websocket(
                    acquire_session_id,
                    &account_id,
                    acquired.entry.clone(),
                    false,
                );
                return Err("WebSocket closed before a terminal event".to_string());
            }
            _ => continue,
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                state.non_transport_error = true;
                drop(ws);
                close_uncached_websocket(&acquired).await;
                release_websocket(
                    acquire_session_id,
                    &account_id,
                    acquired.entry.clone(),
                    false,
                );
                return Err(format!("Invalid Codex WebSocket JSON: {error}"));
            }
        };
        let event_type = parsed
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if event_type == "error" {
            state.api_error_code = extract_codex_event_error(&parsed).0;
        } else if event_type == "response.failed" {
            state.api_error_code = parsed
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if matches!(
            event_type.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            saw_completion = true;
        }

        let event = SseEvent {
            data: parsed.to_string(),
            event: None,
            id: None,
        };
        let normalized = match map_codex_events(
            std::slice::from_ref(&event),
            &mut output,
            options.service_tier.as_deref(),
        ) {
            Ok(normalized) => normalized,
            Err(error) => {
                // Codex event failures are provider/protocol failures, not
                // transport failures. Preserve that distinction so
                // `transport=auto` does not issue a duplicate SSE request
                // after a WebSocket response has already begun failing.
                state.non_transport_error = true;
                drop(ws);
                close_uncached_websocket(&acquired).await;
                release_websocket(
                    acquire_session_id,
                    &account_id,
                    acquired.entry.clone(),
                    false,
                );
                return Err(error);
            }
        };
        if !normalized.is_empty() {
            if !state.started {
                state.started = true;
                push(AssistantMessageEvent::Start {
                    partial: new_output(model),
                });
            }
            if let Err(error) = process_responses_stream_chunk(
                &mut stream_state,
                &normalized,
                &mut output,
                push,
                model,
                &proc_options,
            ) {
                drop(ws);
                close_uncached_websocket(&acquired).await;
                release_websocket(
                    acquire_session_id,
                    &account_id,
                    acquired.entry.clone(),
                    false,
                );
                state.non_transport_error = true;
                return Err(error);
            }
        }
        if stream_state.saw_terminal_response_event() {
            break;
        }
    }

    // assertSuccessfulOutput: pending / error / aborted are stream failures.
    if output.stop_reason() == Some(StopReason::Pending) {
        drop(ws);
        close_uncached_websocket(&acquired).await;
        release_websocket(
            acquire_session_id,
            &account_id,
            acquired.entry.clone(),
            false,
        );
        return Err("Codex stream ended without a stop reason".to_string());
    }
    if output.stop_reason() == Some(StopReason::Error)
        || output.stop_reason() == Some(StopReason::Aborted)
    {
        let known = output.error_message().unwrap_or("").to_string();
        drop(ws);
        close_uncached_websocket(&acquired).await;
        release_websocket(
            acquire_session_id,
            &account_id,
            acquired.entry.clone(),
            false,
        );
        return Err(if known.is_empty() {
            "An unknown error occurred".to_string()
        } else {
            known
        });
    }
    drop(ws);
    close_uncached_websocket(&acquired).await;
    if use_cached_context {
        if let Some(entry) = &acquired.entry {
            if let Some(response_id) = output.response_id() {
                let response_context = Context {
                    system_prompt: None,
                    messages: vec![Message::Assistant(output.clone())],
                    tools: vec![],
                };
                let response_items = match convert_responses_messages(
                    model,
                    &response_context,
                    &CODEX_TOOL_CALL_PROVIDERS,
                    &ConvertResponsesMessagesOptions {
                        include_system_prompt: false,
                        grammar_tool_input_properties,
                        ..Default::default()
                    },
                ) {
                    Ok(items) => items,
                    Err(error) => {
                        release_websocket(
                            acquire_session_id,
                            &account_id,
                            acquired.entry.clone(),
                            false,
                        );
                        return Err(error);
                    }
                }
                .into_iter()
                .filter(|item| {
                    !matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
                })
                .collect();
                entry
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .continuation = Some(CachedWebSocketContinuationState {
                    last_request_body: body.clone(),
                    last_response_id: response_id.to_string(),
                    last_response_items: response_items,
                });
            }
        }
    }
    release_websocket(acquire_session_id, &account_id, acquired.entry, true);
    Ok(output)
}

/// Compatibility wrapper used by focused unit tests that do not need to
/// inspect whether a WebSocket had already emitted response events.
#[cfg(test)]
async fn run_stream_ws(
    model: &Model,
    context: &Context,
    api_key: &str,
    options: &OpenAICodexResponsesOptions,
    body: &Value,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
) -> Result<AssistantMessage, String> {
    let mut state = WebSocketStreamState::default();
    run_stream_ws_with_state(model, context, api_key, options, body, &mut state, push).await
}

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
    let cache_session_id =
        if options.base.cache_retention.as_deref() == Some(crate::types::CACHE_RETENTION_NONE) {
            None
        } else {
            clamp_openai_prompt_cache_key(options.base.session_id.as_deref())
        };
    let body = build_request_body(model, context, options, cache_session_id.as_deref())?;
    let body = apply_payload_hook(body, model, options.base.on_payload.as_ref()).await;
    if is_aborted_signal(options.base.abort_signal.as_ref()) {
        return Err(REQUEST_ABORTED.to_string());
    }
    let headers = build_codex_headers(
        model.headers.as_ref(),
        options.base.base.headers.as_ref(),
        &account_id,
        api_key,
        cache_session_id.as_deref(),
    );
    let url = resolve_codex_url(Some(&model.base_url));
    let http_timeout_ms = options.base.base.timeout_ms;

    // WebSocket transport first (upstream `transport: "auto"` tries WS before
    // falling back to SSE). "sse" forces the SSE path.
    let transport = effective_transport(options);
    let websocket_disabled_for_session =
        transport != "sse" && is_websocket_sse_fallback_active(cache_session_id.as_deref());
    if websocket_disabled_for_session {
        record_websocket_sse_fallback(cache_session_id.as_deref());
    }
    if transport != "sse" && !websocket_disabled_for_session {
        let mut retried_missing_continuation = false;
        let mut retried_connection_limit = false;
        loop {
            let mut websocket_state = WebSocketStreamState::default();
            match run_stream_ws_with_state(
                model,
                context,
                api_key,
                options,
                &body,
                &mut websocket_state,
                push,
            )
            .await
            {
                Ok(output) => return Ok(output),
                Err(ws_error) => {
                    if (is_previous_response_not_found_error(&ws_error)
                        || websocket_state.api_error_code.as_deref()
                            == Some("previous_response_not_found"))
                        && !retried_missing_continuation
                    {
                        retried_missing_continuation = true;
                        continue;
                    }
                    if is_aborted_signal(options.base.abort_signal.as_ref())
                        || ws_error == REQUEST_ABORTED
                    {
                        return Err(REQUEST_ABORTED.to_string());
                    }
                    // Connection-limit errors retry once on a fresh socket;
                    // other transport failures fall back to SSE.
                    let connection_limit = websocket_state.api_error_code.as_deref()
                        == Some("websocket_connection_limit_reached")
                        || ws_error.contains("websocket_connection_limit_reached");
                    if connection_limit && !retried_connection_limit {
                        retried_connection_limit = true;
                        continue;
                    }
                    // Once Codex has emitted a response event, falling back
                    // to SSE would duplicate the request and can produce two
                    // assistant messages. Event/protocol failures are also
                    // terminal even if they happen before the first event.
                    // The one exception is a connection-limit response,
                    // which is a pre-stream transport admission failure and
                    // may fall back after its single retry.
                    if websocket_state.started
                        || (websocket_state.non_transport_error && !connection_limit)
                    {
                        if websocket_state.started && !websocket_state.non_transport_error {
                            record_websocket_failure(cache_session_id.as_deref(), &ws_error);
                        }
                        return Err(ws_error);
                    }
                    record_websocket_failure(cache_session_id.as_deref(), &ws_error);
                    record_websocket_sse_fallback(cache_session_id.as_deref());
                    break;
                }
            }
        }
    }

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
    let body_json = serde_json::to_vec(&body)
        .map_err(|error| format!("Failed to serialize Codex request: {error}"))?;
    builder = match compress_request_body_zstd(&body_json) {
        Some(compressed) => builder
            .header(reqwest::header::CONTENT_ENCODING, "zstd")
            .body(compressed),
        None => builder.body(body_json),
    };
    let request = builder
        .build()
        .map_err(|e| format!("Failed to build Codex request: {e}"))?;

    let max_retries = options.base.base.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
    let mut attempt = 0u32;
    let mut response: Option<reqwest::Response> = None;
    let mut last_error: Option<String> = None;

    while attempt <= max_retries {
        let Some(req) = request.try_clone() else {
            last_error = Some("Codex request body is not cloneable".to_string());
            break;
        };
        // The header timeout bounds only the initial fetch (response headers).
        let send_result = if let Some(signal) = options.base.abort_signal.clone() {
            tokio::select! {
                result = execute_codex_request(&client, req, http_timeout_ms) => result,
                _ = wait_for_abort(signal) => Err(REQUEST_ABORTED.to_string()),
            }
        } else {
            execute_codex_request(&client, req, http_timeout_ms).await
        };

        match send_result {
            Ok(res) => {
                let status = res.status();
                let provider_headers: BTreeMap<String, String> = res
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_lowercase(),
                            value.to_str().unwrap_or("").to_string(),
                        )
                    })
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
                let error_text =
                    match read_codex_response_text(res, options.base.abort_signal.clone()).await {
                        Ok(text) => text,
                        Err(error) => {
                            last_error = Some(error);
                            break;
                        }
                    };
                if attempt < max_retries && is_retryable_error(status.as_u16(), &error_text) {
                    let delay = match get_retry_after_delay_ms(&provider_headers) {
                        Some(delay) => validate_retry_delay_ms(delay, &options.base)?,
                        None => BASE_DELAY_MS * 2u64.pow(attempt),
                    };
                    if let Err(error) = sleep_ms(delay, options.base.abort_signal.clone()).await {
                        last_error = Some(error);
                        break;
                    }
                    attempt += 1;
                    continue;
                }
                let status_text = status.canonical_reason().unwrap_or("").to_string();
                let (message, friendly_message) =
                    parse_error_response(&error_text, status.as_u16(), &status_text);
                last_error = Some(friendly_message.unwrap_or(message));
                break;
            }
            Err(err) => {
                let text = err;
                last_error = Some(text.clone());
                if attempt < max_retries && !text.contains("usage limit") {
                    let delay = BASE_DELAY_MS * 2u64.pow(attempt);
                    if let Err(error) = sleep_ms(delay, options.base.abort_signal.clone()).await {
                        last_error = Some(error);
                        break;
                    }
                    attempt += 1;
                    continue;
                }
                break;
            }
        }
    }

    let response =
        response.ok_or_else(|| last_error.unwrap_or_else(|| "Failed after retries".to_string()))?;

    push(AssistantMessageEvent::Start {
        partial: new_output(model),
    });
    let proc_options = ProcessResponsesOptions {
        service_tier: options.service_tier.clone(),
        grammar_tool_input_properties: create_codex_grammar_properties(model, context)?,
    };
    process_codex_sse_stream(
        response,
        &mut output,
        push,
        model,
        &proc_options,
        options.service_tier.as_deref(),
        options.base.abort_signal.clone(),
    )
    .await?;

    // assertSuccessfulOutput: pending / error / aborted are stream failures.
    if output.stop_reason() == Some(StopReason::Pending) {
        return Err("Codex stream ended without a stop reason".to_string());
    }
    if output.stop_reason() == Some(StopReason::Error)
        || output.stop_reason() == Some(StopReason::Aborted)
    {
        let known = output.error_message().unwrap_or("").to_string();
        return Err(if known.is_empty() {
            "An unknown error occurred".to_string()
        } else {
            known
        });
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
    let Some(sender) = stream.sender() else {
        return stream;
    };
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
                pusher.push(AssistantMessageEvent::Done {
                    reason,
                    message: output.clone(),
                });
                pusher.end(Some(output));
            }
            Err(error_message) => {
                let mut message = new_output(&model);
                let aborted = is_aborted_signal(options.base.abort_signal.as_ref())
                    || error_message == REQUEST_ABORTED;
                message.set_stop_reason(if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                });
                set_output_error_message(
                    &mut message,
                    redact_codex_error(&error_message, &[api_key.as_str()]),
                );
                pusher.push(AssistantMessageEvent::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
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
        transport: None,
    };
    stream(model, context, client, api_key, &go)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        let terminal_type = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut events = vec![
            r#"data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#.to_string(),
            r#"data: {"type":"response.content_part.added","part":{"type":"output_text","text":""}}"#.to_string(),
            r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#.to_string(),
            r#"data: {"type":"response.output_item.done","item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello"}]}}"#.to_string(),
        ];
        let end_turn_json = end_turn
            .map(|v| format!(",\"end_turn\":{v}"))
            .unwrap_or_default();
        let incomplete = if status == "incomplete" {
            r#","incomplete_details":{"reason":"max_output_tokens"}"#
        } else {
            ""
        };
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
        assert_eq!(
            resolve_codex_url(None),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://chatgpt.com/backend-api")),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://chatgpt.com/backend-api/")),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://example.com/codex")),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://example.com/codex/responses")),
            "https://example.com/codex/responses"
        );
    }

    #[test]
    fn extracts_account_id_from_jwt() {
        let token = mock_token("acc_test");
        assert_eq!(extract_account_id(&token).unwrap(), "acc_test");
        assert!(extract_account_id("not-a-jwt").is_err());
        assert!(extract_account_id("aaa.!!!.bbb").is_err());
        let no_claim = format!("aaa.{}.bbb", {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(r#"{"sub":"x"}"#)
        });
        assert!(extract_account_id(&no_claim).is_err());
    }

    #[test]
    fn builds_sse_headers_with_session_affinity() {
        let token = mock_token("acc_test");
        let headers = build_codex_headers(None, None, "acc_test", &token, None);
        assert_eq!(
            headers.get("authorization").unwrap(),
            &format!("Bearer {token}")
        );
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acc_test");
        assert_eq!(headers.get("originator").unwrap(), "pi");
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
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
        request_headers.insert(
            "X-Client-Request-Id".to_string(),
            Some("override".to_string()),
        );
        let headers = build_codex_headers(
            Some(&model_headers),
            Some(&request_headers),
            "acc_test",
            &token,
            None,
        );
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
        assert!(!input
            .iter()
            .any(|m| m.get("role") == Some(&Value::String("developer".to_string()))));
        assert_eq!(input[0]["role"], "user");
        assert!(!body.as_object().unwrap().contains_key("prompt_cache_key"));
        assert!(!body.as_object().unwrap().contains_key("reasoning"));
    }

    #[test]
    fn empty_system_prompt_uses_codex_default() {
        let model = codex_model("gpt-5.5");
        let mut context = codex_ctx();
        context.system_prompt = Some(String::new());

        let body = build_request_body(
            &model,
            &context,
            &OpenAICodexResponsesOptions::default(),
            None,
        )
        .unwrap();

        assert_eq!(body["instructions"], "You are a helpful assistant.");
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
            transport: None,
        };
        let body = build_request_body(&model, &context, &options, Some("session-123")).unwrap();
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["text"]["verbosity"], "high");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["prompt_cache_key"], "session-123");
        // minimal reasoning effort maps through the model's thinking-level map
        // to "low" (port of the upstream "clamps minimal to low" test).
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "low", "summary": "detailed" })
        );

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
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "xhigh", "summary": "auto" })
        );
    }

    #[test]
    fn cached_websocket_body_uses_only_new_input_items() {
        let first_input = json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": "hello" }]
        });
        let response_item = json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "Hi", "annotations": [] }],
            "status": "completed",
            "id": "msg_pi_1"
        });
        let new_input = json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": "continue" }]
        });
        let continuation = CachedWebSocketContinuationState {
            last_request_body: json!({
                "model": "gpt-5.5",
                "store": false,
                "input": [first_input.clone()]
            }),
            last_response_id: "resp_1".to_string(),
            last_response_items: vec![response_item.clone()],
        };
        let body = json!({
            "model": "gpt-5.5",
            "store": false,
            "input": [first_input, response_item, new_input.clone()]
        });
        assert_eq!(
            get_cached_websocket_input_delta(&body, &continuation),
            Some(vec![new_input])
        );

        let changed_request = json!({
            "model": "gpt-5.5",
            "store": false,
            "text": { "verbosity": "high" },
            "input": [json!({ "role": "user", "content": "hello" })]
        });
        assert!(get_cached_websocket_input_delta(&changed_request, &continuation).is_none());
    }

    #[test]
    fn cached_websocket_eviction_guards_match_upstream_ttl_rules() {
        let now = Instant::now();
        assert!(websocket_session_expired(
            now.checked_sub(SESSION_WEBSOCKET_MAX_AGE).unwrap(),
            now
        ));
        assert!(!websocket_session_expired(
            now.checked_sub(SESSION_WEBSOCKET_MAX_AGE - Duration::from_secs(1))
                .unwrap(),
            now
        ));
        assert!(should_evict_idle_websocket(false, 4, 4));
        assert!(!should_evict_idle_websocket(true, 4, 4));
        assert!(!should_evict_idle_websocket(false, 5, 4));
    }

    #[test]
    fn websocket_failure_stats_and_reset_match_upstream_debug_surface() {
        let session_id = format!("ws-debug-{}", uuid::Uuid::new_v4());
        record_websocket_failure(Some(&session_id), "WebSocket closed 1006");
        record_websocket_sse_fallback(Some(&session_id));

        let stats =
            get_openai_codex_websocket_debug_stats(&session_id).expect("websocket debug stats");
        assert_eq!(stats.websocket_failures, 1);
        assert_eq!(stats.sse_fallbacks, 1);
        assert!(stats.websocket_fallback_active);
        assert_eq!(
            stats.last_websocket_error.as_deref(),
            Some("WebSocket closed 1006")
        );
        assert!(is_websocket_sse_fallback_active(Some(&session_id)));

        reset_openai_codex_websocket_debug_stats(Some(&session_id));
        assert!(get_openai_codex_websocket_debug_stats(&session_id).is_none());
        assert!(!is_websocket_sse_fallback_active(Some(&session_id)));
    }

    #[test]
    fn previous_response_not_found_detection_matches_codex_errors() {
        assert!(is_previous_response_not_found_error(
            "Error Code previous_response_not_found: Previous response not found"
        ));
        assert!(is_previous_response_not_found_error(
            "Previous response with id resp_1 was not found"
        ));
        assert!(!is_previous_response_not_found_error("rate limit exceeded"));
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
    fn codex_grammar_tools_use_custom_responses_shape() {
        let mut tool = crate::types::Tool {
            name: "sample".to_string(),
            description: "sample".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"payload": {"type": "string"}},
                "required": ["payload"]
            }),
            constrained_sampling: None,
        };
        let mut variants = BTreeMap::new();
        variants.insert("openai_lark".to_string(), "start: /[a-z]+/".to_string());
        tool.constrained_sampling = Some(crate::types::ConstrainedSampling::Grammar { variants });
        let out = convert_codex_tools(&[tool], true, true).unwrap();
        assert_eq!(out[0]["type"], "custom");
        assert_eq!(out[0]["format"]["syntax"], "lark");
        assert_eq!(out[0]["format"]["definition"], "start: /[a-z]+/");
    }

    #[test]
    fn reuses_shared_message_conversion_without_system_prompt() {
        let model = codex_model("gpt-5.5");
        let context = codex_ctx();
        let messages = convert_responses_messages(
            &model,
            &context,
            &CODEX_TOOL_CALL_PROVIDERS,
            &ConvertResponsesMessagesOptions {
                include_system_prompt: false,
                ..Default::default()
            },
        )
        .unwrap();
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
        let events = parse_sse(
            r#"data: {"type":"error","code":"server_error","message":"boom"}

"#,
        );
        let mut output = new_output(&model);
        let err = map_codex_events(&events, &mut output, None).unwrap_err();
        assert_eq!(err, "Codex error: boom");
    }

    #[test]
    fn errors_on_response_failed() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(
            r#"data: {"type":"response.failed","response":{"error":{"code":"gone","message":"bad request"}}}

"#,
        );
        let mut output = new_output(&model);
        let err = map_codex_events(&events, &mut output, None).unwrap_err();
        assert_eq!(err, "bad request");
    }

    #[test]
    fn response_failed_code_only_preserves_retryable_code() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(
            r#"data: {"type":"response.failed","response":{"error":{"code":"previous_response_not_found"}}}

"#,
        );
        let mut output = new_output(&model);
        let err = map_codex_events(&events, &mut output, None).unwrap_err();
        assert_eq!(err, "previous_response_not_found");
        assert!(is_previous_response_not_found_error(&err));
    }

    #[test]
    fn normalizes_unknown_statuses_away() {
        let model = codex_model("gpt-5.5");
        let events = parse_sse(
            r#"data: {"type":"response.completed","response":{"status":"bogus","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#,
        );
        let mut output = new_output(&model);
        let normalized = map_codex_events(&events, &mut output, None).unwrap();
        let parsed: Value = serde_json::from_str(&normalized[0].data).unwrap();
        assert!(parsed["response"].get("status").is_none());
    }

    #[test]
    fn resolves_service_tier_default_echo() {
        assert_eq!(
            resolve_codex_service_tier(Some("default"), Some("flex")),
            Some("flex".to_string())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("default"), Some("priority")),
            Some("priority".to_string())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("default"), None),
            Some("default".to_string())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("priority"), Some("flex")),
            Some("priority".to_string())
        );
        assert_eq!(
            resolve_codex_service_tier(None, Some("flex")),
            Some("flex".to_string())
        );
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
        let normalized =
            map_codex_events(&events, &mut output, options.service_tier.as_deref()).unwrap();
        let mut pushed = Vec::new();
        let proc_options = ProcessResponsesOptions {
            service_tier: options.service_tier.clone(),
            grammar_tool_input_properties: create_codex_grammar_properties(
                model,
                &Context::default(),
            )
            .unwrap_or_default(),
        };
        process_responses_stream(
            &normalized,
            &mut output,
            &mut |e| pushed.push(e.clone()),
            model,
            &proc_options,
        )
        .unwrap();
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
        assert!(pushed
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
        assert!(pushed
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextEnd { .. })));
    }

    #[test]
    fn maps_incomplete_to_length_stop() {
        let model = codex_model("gpt-5.5");
        let options = OpenAICodexResponsesOptions::default();
        let (message, _) = process_sse_text(&codex_sse("incomplete", None), &model, &options);
        assert_eq!(message.stop_reason(), Some(StopReason::Length));
        assert_eq!(
            message.raw_stop_reason().unwrap(),
            "incomplete.max_output_tokens"
        );
    }

    #[test]
    fn service_tier_pricing_multiplier_applies_when_backend_echoes_default() {
        // Port of the upstream service-tier pricing matrix.
        for (model_id, service_tier, multiplier) in
            [("gpt-5.5", "flex", 0.5), ("gpt-5.5", "priority", 2.5)]
        {
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
            assert!(
                (cost.input - 1.0 * multiplier).abs() < 1e-9,
                "{model_id} {service_tier}"
            );
            assert!(
                (cost.output - 2.0 * multiplier).abs() < 1e-9,
                "{model_id} {service_tier}"
            );
            assert!(
                (cost.total - 3.0 * multiplier).abs() < 1e-9,
                "{model_id} {service_tier}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Retry / error parsing
    // ------------------------------------------------------------------

    #[test]
    fn retryable_classification() {
        assert!(is_retryable_error(429, "rate limited, try later"));
        assert!(is_retryable_error(500, ""));
        assert!(is_retryable_error(502, ""));
        assert!(is_retryable_error(503, ""));
        assert!(is_retryable_error(504, ""));
        assert!(!is_retryable_error(501, ""));
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
        assert!(friendly
            .unwrap()
            .starts_with("You have hit your ChatGPT usage limit (free plan)."));
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
        headers.insert(
            "retry-after".to_string(),
            "Wed, 21 Oct 2030 07:28:00 GMT".to_string(),
        );
        assert!(get_retry_after_delay_ms(&headers).is_some());
        headers.insert("retry-after".to_string(), "not a date".to_string());
        assert_eq!(get_retry_after_delay_ms(&headers), None);
        assert_eq!(get_retry_after_delay_ms(&BTreeMap::new()), None);
    }

    // ------------------------------------------------------------------
    // Stream entry points
    // ------------------------------------------------------------------

    #[test]
    fn stream_without_key_is_terminal_error() {
        let model = codex_model("gpt-5.5");
        let s = stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            None,
            &OpenAICodexResponsesOptions::default(),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, msg) = rt.block_on(s.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(
            err.contains("No API key for provider: openai-codex"),
            "{err}"
        );
    }

    #[test]
    fn invalid_token_is_a_terminal_error() {
        let model = codex_model("gpt-5.5");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, msg) = rt
            .block_on(async {
                let s = stream(
                    &model,
                    &Context::default(),
                    reqwest::Client::new(),
                    Some("not-a-jwt"),
                    &OpenAICodexResponsesOptions::default(),
                );
                tokio::time::timeout(std::time::Duration::from_secs(5), s.collect()).await
            })
            .expect("timed out waiting for the invalid-token error stream");
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(
            err.contains("Failed to extract accountId from token"),
            "{err}"
        );
    }

    #[test]
    fn preaborted_stream_reports_aborted_terminal_reason() {
        use std::sync::atomic::AtomicBool;

        let model = codex_model("gpt-5.5");
        let signal = Arc::new(AtomicBool::new(true));
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                abort_signal: Some(signal),
                ..Default::default()
            },
            ..Default::default()
        };
        let token = mock_token("acct-aborted");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, message) = rt.block_on(async {
            let stream = stream(
                &model,
                &Context::default(),
                reqwest::Client::new(),
                Some(&token),
                &options,
            );
            tokio::time::timeout(Duration::from_secs(1), stream.collect())
                .await
                .expect("timed out waiting for aborted stream")
        });

        assert!(events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error_message,
            } if error_message.stop_reason() == Some(StopReason::Aborted)
        )));
        assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
        assert_eq!(message.error_message(), Some("Request was aborted"));
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
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "xhigh", "summary": "auto" })
        );
    }

    fn stream_simple_reasoning_effort_for_test(
        model: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<String> {
        let clamped =
            clamp_thinking_level(model, ModelThinkingLevel::from(options.reasoning.unwrap()));
        if clamped == ModelThinkingLevel::Off {
            None
        } else {
            Some(clamped.as_str().to_string())
        }
    }

    // ------------------------------------------------------------------
    // WebSocket transport
    // ------------------------------------------------------------------

    /// Mock Codex WebSocket server: accepts one connection, reads the
    /// `response.create` frame, replies with a scripted event sequence, and
    /// closes. Returns the ws:// base URL.
    async fn mock_codex_ws_server(events: Vec<String>) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut read) = ws.split();
            // Read the response.create frame.
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = read.next().await
            {
                assert!(text.contains("\"type\":\"response.create\""), "got: {text}");
            }
            for event in events {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(
                    event.to_string(),
                ))
                .await
                .unwrap();
            }
            let _ = sink.close().await;
        });
        format!("ws://127.0.0.1:{port}/backend-api/codex/responses")
    }

    async fn mock_codex_incremental_ws_server() -> (String, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut read) = ws.split();
            let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = read.next().await
            else {
                return;
            };
            assert!(text.contains("\"type\":\"response.create\""), "got: {text}");

            let events = ws_codex_events("completed");
            for event in events.iter().take(3) {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(event.clone()))
                    .await
                    .unwrap();
            }
            let _ = release_receiver.await;
            for event in events.iter().skip(3) {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(event.clone()))
                    .await
                    .unwrap();
            }
            let _ = sink.close().await;
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            release_sender,
        )
    }

    async fn mock_hanging_codex_ws_server(
        complete_handshake: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            if complete_handshake {
                use futures_util::StreamExt as _;
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = ws.next().await;
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(ws);
                return;
            }
            std::future::pending::<()>().await;
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            task,
        )
    }

    async fn mock_codex_busy_ws_server() -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
        Arc<Mutex<Vec<usize>>>,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let connections = Arc::new(Mutex::new(Vec::new()));
        let server_connections = connections.clone();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            let (first_stream, _) = listener.accept().await.unwrap();
            let first_ws = tokio_tungstenite::accept_async(first_stream).await.unwrap();
            let first_connections = server_connections.clone();
            tokio::spawn(async move {
                let (mut sink, mut read) = first_ws.split();
                if read.next().await.is_none() {
                    return;
                }
                first_connections
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(1);
                let _ = first_seen_tx.send(());
                let _ = release_rx.await;
                for event in ws_cached_events("resp_busy_1", "msg_busy_1", "First") {
                    let _ = sink
                        .send(tokio_tungstenite::tungstenite::Message::Text(event))
                        .await;
                }
            });

            let (second_stream, _) = listener.accept().await.unwrap();
            let second_ws = tokio_tungstenite::accept_async(second_stream)
                .await
                .unwrap();
            let (mut sink, mut read) = second_ws.split();
            if read.next().await.is_none() {
                return;
            }
            server_connections
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(2);
            for event in ws_cached_events("resp_busy_2", "msg_busy_2", "Second") {
                let _ = sink
                    .send(tokio_tungstenite::tungstenite::Message::Text(event))
                    .await;
            }
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            first_seen_rx,
            release_tx,
            connections,
        )
    }

    async fn mock_codex_ws_failure_then_sse_server() -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (ws_stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(ws_stream).await.unwrap();
            let _ = ws.next().await;
            let _ = ws
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await;

            let (mut http, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = http.read(&mut chunk).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = ws_codex_events("completed")
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            http.write_all(response.as_bytes()).await.unwrap();
            let _ = http.shutdown().await;
        });
        format!("http://127.0.0.1:{port}/backend-api")
    }

    async fn mock_codex_connection_limit_retry_server() -> (String, Arc<Mutex<usize>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(Mutex::new(0_usize));
        let server_connections = connections.clone();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            for connection in 1..=2 {
                let (stream, _) = listener.accept().await.unwrap();
                *server_connections
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) += 1;
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = ws.next().await;
                if connection == 1 {
                    ws.send(tokio_tungstenite::tungstenite::Message::Text(
                        r#"{"type":"error","error":{"code":"websocket_connection_limit_reached","message":"limit"}}"#.to_string(),
                    ))
                    .await
                    .unwrap();
                    let _ = ws.close(None).await;
                } else {
                    for event in ws_cached_events("resp_limit", "msg_limit", "Recovered") {
                        ws.send(tokio_tungstenite::tungstenite::Message::Text(event))
                            .await
                            .unwrap();
                    }
                }
            }
        });
        (format!("http://127.0.0.1:{port}/backend-api"), connections)
    }

    fn ws_cached_events(response_id: &str, message_id: &str, text: &str) -> Vec<String> {
        vec![
            format!(r#"{{"type":"response.created","response":{{"id":"{response_id}"}}}}"#),
            format!(
                r#"{{"type":"response.output_item.added","item":{{"type":"message","id":"{message_id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
            r#"{"type":"response.content_part.added","part":{"type":"output_text","text":""}}"#
                .to_string(),
            format!(r#"{{"type":"response.output_text.delta","delta":"{text}"}}"#),
            format!(
                r#"{{"type":"response.output_item.done","item":{{"type":"message","id":"{message_id}","role":"assistant","status":"completed","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"response.completed","response":{{"id":"{response_id}","status":"completed","usage":{{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}}}}"#
            ),
        ]
    }

    async fn mock_codex_cached_ws_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>)
    {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut read) = ws.split();
            for index in 1..=2 {
                let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    read.next().await
                else {
                    return;
                };
                requests_for_server
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(serde_json::from_str(&text).unwrap());
                let response_id = format!("resp_{index}");
                let message_id = format!("msg_{index}");
                let text = if index == 1 { "Hello" } else { "Again" };
                for event in ws_cached_events(&response_id, &message_id, text) {
                    sink.send(tokio_tungstenite::tungstenite::Message::Text(event))
                        .await
                        .unwrap();
                }
            }
            let _ = sink.close().await;
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            requests,
        )
    }

    async fn mock_codex_missing_continuation_ws_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(usize, Value)>>>,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            use futures_util::{SinkExt as _, StreamExt as _};
            let (first_stream, _) = listener.accept().await.unwrap();
            let first_ws = tokio_tungstenite::accept_async(first_stream).await.unwrap();
            let first_requests = requests_for_server.clone();
            tokio::spawn(async move {
                let (mut sink, mut read) = first_ws.split();
                for request_index in 0..2 {
                    let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                        read.next().await
                    else {
                        return;
                    };
                    first_requests
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push((1, serde_json::from_str(&text).unwrap()));
                    if request_index == 0 {
                        for event in ws_cached_events("resp_1", "msg_1", "Hello") {
                            sink.send(tokio_tungstenite::tungstenite::Message::Text(event))
                                .await
                                .unwrap();
                        }
                    } else {
                        sink.send(tokio_tungstenite::tungstenite::Message::Text(
                            r#"{"type":"error","code":"previous_response_not_found","message":"Previous response not found"}"#.to_string(),
                        ))
                        .await
                        .unwrap();
                        let _ = sink.close().await;
                        return;
                    }
                }
            });

            let (second_stream, _) = listener.accept().await.unwrap();
            let second_ws = tokio_tungstenite::accept_async(second_stream)
                .await
                .unwrap();
            let (mut sink, mut read) = second_ws.split();
            let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = read.next().await
            else {
                return;
            };
            requests_for_server
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((2, serde_json::from_str(&text).unwrap()));
            for event in ws_cached_events("resp_2", "msg_2", "Recovered") {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(event))
                    .await
                    .unwrap();
            }
            let _ = sink.close().await;
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            requests,
        )
    }

    async fn serve_codex_ws_connection(
        stream: tokio::net::TcpStream,
        connection_id: usize,
        responses: Vec<(&'static str, &'static str, &'static str)>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    ) {
        use futures_util::{SinkExt as _, StreamExt as _};
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut read) = ws.split();
        for (response_id, message_id, text) in responses {
            let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_))) = read.next().await
            else {
                return;
            };
            requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(connection_id);
            for event in ws_cached_events(response_id, message_id, text) {
                sink.send(tokio_tungstenite::tungstenite::Message::Text(event))
                    .await
                    .unwrap();
            }
        }
        let _ = sink.close().await;
    }

    async fn mock_codex_account_scoped_ws_server(
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<usize>>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests_for_server = requests.clone();
        tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.unwrap();
            let first_requests = requests_for_server.clone();
            tokio::spawn(serve_codex_ws_connection(
                first_stream,
                1,
                vec![("resp_a1", "msg_a1", "A1"), ("resp_a2", "msg_a2", "A2")],
                first_requests,
            ));
            let (second_stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve_codex_ws_connection(
                second_stream,
                2,
                vec![("resp_b1", "msg_b1", "B1")],
                requests_for_server,
            ));
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            requests,
        )
    }

    fn ws_codex_events(status: &str) -> Vec<String> {
        let terminal_type = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let incomplete = if status == "incomplete" {
            r#","incomplete_details":{"reason":"max_output_tokens"}"#
        } else {
            ""
        };
        vec![
            r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#.to_string(),
            r#"{"type":"response.content_part.added","part":{"type":"output_text","text":""}}"#.to_string(),
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#.to_string(),
            r#"{"type":"response.output_item.done","item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello"}]}}"#.to_string(),
            format!(r#"{{"type":"{terminal_type}","response":{{"status":"{status}"{incomplete},"usage":{{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{{"cached_tokens":0}}}}}}}}"#),
        ]
    }

    #[tokio::test]
    async fn websocket_transport_streams_and_completes() {
        let base = mock_codex_ws_server(ws_codex_events("completed")).await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let token = mock_token("acct-1");
        let options = OpenAICodexResponsesOptions {
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let context = codex_ctx();
        let body = build_request_body(&model, &context, &options, None).expect("ws body");
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let output = run_stream_ws(&model, &context, &token, &options, &body, &mut |e| {
            events.push(e)
        })
        .await
        .expect("ws stream");
        let text: String = output
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Start { .. })));
    }

    #[tokio::test]
    async fn websocket_connect_and_idle_timeouts_are_distinct() {
        let token = mock_token("acct-timeout");
        let context = codex_ctx();

        let (connect_base, connect_task) = mock_hanging_codex_ws_server(false).await;
        let mut connect_model = codex_model("gpt-5.4-codex");
        connect_model.base_url = connect_base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let connect_options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                websocket_connect_timeout_ms: Some(25),
                ..Default::default()
            },
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let connect_body = build_request_body(&connect_model, &context, &connect_options, None)
            .expect("connect timeout body");
        let connect_error = run_stream_ws(
            &connect_model,
            &context,
            &token,
            &connect_options,
            &connect_body,
            &mut |_| {},
        )
        .await
        .expect_err("connect timeout");
        connect_task.abort();
        assert_eq!(connect_error, "WebSocket connect timed out after 25ms");

        let (idle_base, idle_task) = mock_hanging_codex_ws_server(true).await;
        let mut idle_model = codex_model("gpt-5.4-codex");
        idle_model.base_url = idle_base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let idle_options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    timeout_ms: Some(25),
                    ..Default::default()
                },
                ..Default::default()
            },
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let idle_body = build_request_body(&idle_model, &context, &idle_options, None)
            .expect("idle timeout body");
        let idle_error = run_stream_ws(
            &idle_model,
            &context,
            &token,
            &idle_options,
            &idle_body,
            &mut |_| {},
        )
        .await
        .expect_err("idle timeout");
        idle_task.abort();
        assert_eq!(idle_error, "WebSocket idle timeout after 25ms");

        let (abort_base, abort_task) = mock_hanging_codex_ws_server(true).await;
        let mut abort_model = codex_model("gpt-5.4-codex");
        abort_model.base_url = abort_base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let abort_signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let abort_options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                abort_signal: Some(abort_signal.clone()),
                ..Default::default()
            },
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let abort_body =
            build_request_body(&abort_model, &context, &abort_options, None).expect("abort body");
        let aborting = tokio::spawn(async move {
            run_stream_ws(
                &abort_model,
                &context,
                &token,
                &abort_options,
                &abort_body,
                &mut |_| {},
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        abort_signal.store(true, Ordering::SeqCst);
        assert_eq!(
            aborting.await.unwrap().expect_err("abort active websocket"),
            REQUEST_ABORTED
        );
        abort_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_busy_cached_connection_uses_an_isolated_socket() {
        let (base, first_seen, release_first, connections) = mock_codex_busy_ws_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let session_id = format!("ws-busy-{}", uuid::Uuid::new_v4());
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                session_id: Some(session_id.clone()),
                ..Default::default()
            },
            transport: Some("websocket-cached".to_string()),
            ..Default::default()
        };
        let context = codex_ctx();
        let body = build_request_body(&model, &context, &options, Some(&session_id))
            .expect("busy websocket body");

        let first_model = model.clone();
        let first_context = context.clone();
        let first_options = options.clone();
        let first_body = body.clone();
        let first_token = mock_token("acct-busy");
        let first = tokio::spawn(async move {
            run_stream_ws(
                &first_model,
                &first_context,
                &first_token,
                &first_options,
                &first_body,
                &mut |_| {},
            )
            .await
        });
        first_seen.await.expect("first request reached server");

        let second = run_stream_ws(
            &model,
            &context,
            &mock_token("acct-busy"),
            &options,
            &body,
            &mut |_| {},
        )
        .await
        .expect("second request on isolated socket");
        assert_eq!(second.response_id(), Some("resp_busy_2"));
        let _ = release_first.send(());
        assert_eq!(
            first.await.unwrap().expect("first request").response_id(),
            Some("resp_busy_1")
        );
        assert_eq!(
            *connections
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            vec![1, 2]
        );
        let stats = get_openai_codex_websocket_debug_stats(&session_id).expect("busy stats");
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.connections_created, 2);
        assert_eq!(stats.connections_reused, 0);
        close_openai_codex_websocket_sessions(Some(&session_id));
        reset_openai_codex_websocket_debug_stats(Some(&session_id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_transport_emits_delta_before_terminal_frame() {
        let (base, release) = mock_codex_incremental_ws_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let token = mock_token("acct-incremental");
        let options = OpenAICodexResponsesOptions {
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let context = codex_ctx();
        let body = build_request_body(&model, &context, &options, None).expect("ws body");
        let observed = Arc::new(Mutex::new(Vec::<AssistantMessageEvent>::new()));
        let observed_for_task = observed.clone();
        let task = tokio::spawn(async move {
            run_stream_ws(&model, &context, &token, &options, &body, &mut |event| {
                observed_for_task
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event)
            })
            .await
        });

        let emitted_before_terminal = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if observed.lock().unwrap_or_else(|error| error.into_inner()).iter().any(|event| {
                    matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "Hello")
                }) {
                    break true;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or(false);
        let _ = release.send(());
        let output = task.await.unwrap().expect("incremental ws stream");

        assert!(emitted_before_terminal);
        assert_eq!(
            output
                .content()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "Hello"
        );
    }

    #[tokio::test]
    async fn websocket_transport_handles_incomplete() {
        let base = mock_codex_ws_server(ws_codex_events("incomplete")).await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let token = mock_token("acct-1");
        let options = OpenAICodexResponsesOptions {
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let context = codex_ctx();
        let body = build_request_body(&model, &context, &options, None).expect("ws body");
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let output = run_stream_ws(&model, &context, &token, &options, &body, &mut |e| {
            events.push(e)
        })
        .await
        .expect("ws stream");
        let text: String = output
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
    }

    #[tokio::test]
    async fn websocket_cached_reuses_session_socket_and_sends_input_delta() {
        let (base, requests) = mock_codex_cached_ws_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let token = mock_token("acct-cached");
        let session_id = format!("s-009-{}", uuid::Uuid::new_v4());
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    timeout_ms: Some(1_000),
                    ..Default::default()
                },
                session_id: Some(session_id.clone()),
                ..Default::default()
            },
            transport: Some("websocket-cached".to_string()),
            ..Default::default()
        };
        let first_context = codex_ctx();
        let first_body = build_request_body(
            &model,
            &first_context,
            &options,
            options.base.session_id.as_deref(),
        )
        .expect("first cached body");
        let first = tokio::time::timeout(
            Duration::from_secs(3),
            run_stream_ws(
                &model,
                &first_context,
                &token,
                &options,
                &first_body,
                &mut |_| {},
            ),
        )
        .await
        .expect("first cached request timed out")
        .expect("first cached request failed");
        let mut second_context = first_context.clone();
        second_context.messages.push(Message::Assistant(first));
        second_context
            .messages
            .push(Message::User(UserContent::string("continue", 2)));
        let second_body = build_request_body(
            &model,
            &second_context,
            &options,
            options.base.session_id.as_deref(),
        )
        .expect("second cached body");
        tokio::time::timeout(
            Duration::from_secs(3),
            run_stream_ws(
                &model,
                &second_context,
                &token,
                &options,
                &second_body,
                &mut |_| {},
            ),
        )
        .await
        .expect("second cached request timed out")
        .expect("second cached request failed");

        let requests = requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["prompt_cache_key"], session_id);
        assert_eq!(requests[0]["store"], false);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[1]["previous_response_id"], "resp_1");
        assert_eq!(requests[1]["store"], false);
        assert_eq!(
            requests[1]["input"],
            json!([{
                "role": "user",
                "content": [{ "type": "input_text", "text": "continue" }]
            }])
        );
        let stats =
            get_openai_codex_websocket_debug_stats(&session_id).expect("cached websocket stats");
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.connections_created, 1);
        assert_eq!(stats.connections_reused, 1);
        assert_eq!(stats.cached_context_requests, 2);
        assert_eq!(stats.full_context_requests, 1);
        assert_eq!(stats.delta_requests, 1);
        assert_eq!(stats.last_delta_input_items, Some(1));
        assert_eq!(stats.last_previous_response_id.as_deref(), Some("resp_1"));
        assert_eq!(stats.store_true_requests, 0);

        close_openai_codex_websocket_sessions(Some(&session_id));
        assert!(!WEBSOCKET_SESSION_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&session_id));
        reset_openai_codex_websocket_debug_stats(Some(&session_id));
        assert!(get_openai_codex_websocket_debug_stats(&session_id).is_none());
        assert!(!is_websocket_sse_fallback_active(Some(&session_id)));
    }

    #[tokio::test]
    async fn websocket_cached_reopens_after_missing_previous_response() {
        let (base, requests) = mock_codex_missing_continuation_ws_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let token = mock_token("acct-recovery");
        let session_id = format!("s-009-recovery-{}", uuid::Uuid::new_v4());
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    timeout_ms: Some(1_000),
                    ..Default::default()
                },
                session_id: Some(session_id),
                ..Default::default()
            },
            transport: Some("websocket-cached".to_string()),
            ..Default::default()
        };
        let first_context = codex_ctx();
        let first = run_stream(
            &model,
            &first_context,
            reqwest::Client::new(),
            &token,
            &options,
            &mut |_| {},
        )
        .await
        .expect("first recovery request");
        let mut second_context = first_context;
        second_context.messages.push(Message::Assistant(first));
        second_context
            .messages
            .push(Message::User(UserContent::string("continue", 2)));
        let recovered = tokio::time::timeout(
            Duration::from_secs(3),
            run_stream(
                &model,
                &second_context,
                reqwest::Client::new(),
                &token,
                &options,
                &mut |_| {},
            ),
        )
        .await
        .expect("recovery request timed out")
        .expect("recovery request failed");
        assert_eq!(recovered.response_id(), Some("resp_2"));

        let requests = requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, 1);
        assert_eq!(requests[1].0, 1);
        assert_eq!(requests[2].0, 2);
        assert_eq!(requests[1].1["previous_response_id"], "resp_1");
        assert!(requests[2].1.get("previous_response_id").is_none());
        assert_eq!(requests[2].1["input"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn websocket_session_cache_is_scoped_by_authenticated_account() {
        let (base, requests) = mock_codex_account_scoped_ws_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base
            .replace("ws://", "http://")
            .replace("/codex/responses", "");
        let session_id = format!("s-009-account-{}", uuid::Uuid::new_v4());
        let options = || OpenAICodexResponsesOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    timeout_ms: Some(1_000),
                    ..Default::default()
                },
                session_id: Some(session_id.clone()),
                ..Default::default()
            },
            transport: Some("websocket".to_string()),
            ..Default::default()
        };
        let context = codex_ctx();
        let first_options = options();
        let body = build_request_body(
            &model,
            &context,
            &first_options,
            first_options.base.session_id.as_deref(),
        )
        .expect("first account body");
        run_stream_ws(
            &model,
            &context,
            &mock_token("account-a"),
            &first_options,
            &body,
            &mut |_| {},
        )
        .await
        .expect("first account request");
        let second_options = options();
        let body = build_request_body(
            &model,
            &context,
            &second_options,
            second_options.base.session_id.as_deref(),
        )
        .expect("second account body");
        run_stream_ws(
            &model,
            &context,
            &mock_token("account-b"),
            &second_options,
            &body,
            &mut |_| {},
        )
        .await
        .expect("second account request");
        let third_options = options();
        let body = build_request_body(
            &model,
            &context,
            &third_options,
            third_options.base.session_id.as_deref(),
        )
        .expect("third account body");
        run_stream_ws(
            &model,
            &context,
            &mock_token("account-a"),
            &third_options,
            &body,
            &mut |_| {},
        )
        .await
        .expect("reused first account request");

        assert_eq!(
            *requests.lock().unwrap_or_else(|error| error.into_inner()),
            vec![1, 2, 1]
        );
    }

    #[tokio::test]
    async fn websocket_transport_connection_failure_falls_back_to_sse() {
        // A ws:// URL with no listener: the WS connect fails and the SSE path
        // must be attempted (which also fails against the dead port, but the
        // error must come from the SSE attempt, not a WS-only panic).
        let model = codex_model("gpt-5.4-codex");
        let token = mock_token("acct-1");
        let options = OpenAICodexResponsesOptions {
            transport: Some("auto".to_string()),
            ..Default::default()
        };
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let result = run_stream(
            &model,
            &codex_ctx(),
            reqwest::Client::new(),
            &token,
            &options,
            &mut |e| events.push(e),
        )
        .await;
        // WS connect fails -> SSE attempt against the same dead port fails.
        assert!(
            result.is_err(),
            "expected transport failure, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn websocket_transport_failure_uses_sse_and_activates_session_circuit() {
        let base = mock_codex_ws_failure_then_sse_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base;
        let session_id = format!("ws-fallback-{}", uuid::Uuid::new_v4());
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                session_id: Some(session_id.clone()),
                ..Default::default()
            },
            transport: Some("auto".to_string()),
            ..Default::default()
        };
        let output = run_stream(
            &model,
            &codex_ctx(),
            reqwest::Client::new(),
            &mock_token("acct-fallback"),
            &options,
            &mut |_| {},
        )
        .await
        .expect("SSE fallback succeeds");
        assert_eq!(output.stop_reason(), Some(StopReason::Stop));
        let stats = get_openai_codex_websocket_debug_stats(&session_id).expect("fallback stats");
        assert_eq!(stats.websocket_failures, 1);
        assert_eq!(stats.sse_fallbacks, 1);
        assert!(stats.websocket_fallback_active);
        assert!(is_websocket_sse_fallback_active(Some(&session_id)));
        reset_openai_codex_websocket_debug_stats(Some(&session_id));
    }

    #[tokio::test]
    async fn websocket_connection_limit_retries_once_on_a_fresh_socket() {
        let (base, connections) = mock_codex_connection_limit_retry_server().await;
        let mut model = codex_model("gpt-5.4-codex");
        model.base_url = base;
        let output = run_stream(
            &model,
            &codex_ctx(),
            reqwest::Client::new(),
            &mock_token("acct-limit"),
            &OpenAICodexResponsesOptions {
                transport: Some("websocket".to_string()),
                ..Default::default()
            },
            &mut |_| {},
        )
        .await
        .expect("connection-limit retry succeeds");
        assert_eq!(output.response_id(), Some("resp_limit"));
        assert_eq!(
            *connections
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            2
        );
    }

    #[test]
    fn resolves_codex_websocket_urls() {
        assert_eq!(
            resolve_codex_websocket_url(Some("https://chatgpt.com/backend-api")),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_websocket_url(Some("http://127.0.0.1:8080")),
            "ws://127.0.0.1:8080/codex/responses"
        );
    }
}
