//! OpenAI Responses API adaptor — port of
//! `packages/ai/src/api/openai-responses.ts` over the public REST
//! `/responses` SSE endpoint (the openai SDK wraps this surface).
//!
//! Converts the unified context into the Responses request (system prompt as
//! developer role for reasoning models, converted input items, tools,
//! reasoning effort/summary, prompt-cache key clamping), streams SSE events,
//! and emits the unified `AssistantMessageEvent` protocol. `stream` never
//! throws: failures are encoded as a terminal error event.

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{clamp_thinking_level, Model};
use crate::sse::SseParser;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason, ModelThinkingLevel,
    ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions, ToolChoice, Usage,
};

use super::openai_completions::{
    abortable, abortable_with_timeout, apply_payload_hook, error_reason, format_reqwest_error,
    immediate_error_stream, signal_aborted, terminal_error_message, AbortTimeoutError,
};
use super::openai_responses_shared::*;

/// OpenAI Responses rejects max_output_tokens below 16.
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;
const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|k| k.chars().take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH).collect())
}

fn has_header(headers: Option<&crate::types::ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else { return false };
    let expected = name.to_lowercase();
    headers.iter().any(|(key, value)| {
        key.to_lowercase() == expected && value.as_ref().is_some_and(|v| !v.trim().is_empty())
    })
}

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&crate::types::ProviderHeaders>,
) -> Result<String, String> {
    if let Some(key) = api_key {
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    Err(format!("No API key for provider: {provider}"))
}

fn replace_header(headers: &mut ProviderHeaders, name: impl Into<String>, value: Option<String>) {
    let name = name.into();
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
    headers.insert(name, value);
}

fn build_request_headers(
    model: &Model,
    context: &Context,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &OpenAIResponsesCompat,
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
    if model.provider == "github-copilot" {
        let has_images = super::github_copilot_headers::has_copilot_vision_input(&context.messages);
        for (name, value) in super::github_copilot_headers::build_copilot_dynamic_headers(
            &context.messages,
            has_images,
        ) {
            replace_header(&mut headers, name, Some(value));
        }
    }
    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
        if compat.session_affinity_format == "openrouter" {
            replace_header(&mut headers, "x-session-id", Some(session_id.to_string()));
        } else {
            if compat.session_affinity_format == "openai" {
                replace_header(&mut headers, "session_id", Some(session_id.to_string()));
            }
            replace_header(
                &mut headers,
                "x-client-request-id",
                Some(session_id.to_string()),
            );
        }
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            replace_header(&mut headers, name.clone(), value.clone());
        }
    }
    headers
}

fn has_header_name(headers: &ProviderHeaders, name: &str) -> bool {
    headers
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
}

fn detect_session_affinity_format(provider: &str, base_url: &str) -> &'static str {
    if provider == "openrouter" || base_url.contains("openrouter.ai") {
        "openrouter"
    } else {
        "openai"
    }
}

pub struct OpenAIResponsesCompat {
    pub supports_developer_role: bool,
    pub session_affinity_format: String,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
}

impl OpenAIResponsesCompat {
    fn get(model: &Model) -> Self {
        let compat = model.compat.as_ref();
        let get_bool = |key: &str, default: bool| -> bool {
            compat
                .and_then(|c| c.get(key))
                .and_then(|v| v.as_bool())
                .unwrap_or(default)
        };
        Self {
            supports_developer_role: get_bool("supportsDeveloperRole", true),
            session_affinity_format: compat
                .and_then(|c| c.get("sessionAffinityFormat"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    detect_session_affinity_format(&model.provider, &model.base_url).to_string()
                }),
            supports_long_cache_retention: get_bool("supportsLongCacheRetention", true),
            supports_strict_mode: get_bool("supportsStrictMode", false),
            supports_openai_grammar_tools: get_bool("supportsOpenAIGrammarTools", false),
            supports_additional_tools: get_bool("supportsAdditionalTools", false),
            supports_tool_search: get_bool("supportsToolSearch", false),
            supports_explicit_prompt_cache_mode: get_bool("supportsExplicitPromptCacheMode", false),
        }
    }
}

fn resolve_cache_retention(
    cache_retention: Option<&str>,
    env: Option<&crate::types::ProviderEnv>,
) -> String {
    if let Some(value) = cache_retention {
        return value.to_string();
    }
    match super::openai_completions::get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() {
        Some("long") => "long".to_string(),
        _ => "short".to_string(),
    }
}

fn get_prompt_cache_retention(
    compat: &OpenAIResponsesCompat,
    cache_retention: &str,
) -> Option<&'static str> {
    if cache_retention == "long" && compat.supports_long_cache_retention {
        Some("24h")
    } else {
        None
    }
}

/// Options for OpenAI Responses requests (subset of upstream
/// `OpenAIResponsesOptions`).
#[derive(Clone, Default)]
pub struct OpenAIResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
}

impl OpenAIResponsesOptions {
    pub fn from_stream_options(base: StreamOptions) -> Self {
        Self {
            base,
            ..Default::default()
        }
    }
}

/// Assemble the Responses request body (port of `buildParams`).
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenAIResponsesOptions,
) -> Result<Value, String> {
    let compat = OpenAIResponsesCompat::get(model);
    let grammar_properties = super::constrained_sampling::create_grammar_tool_input_properties(
        &context.tools,
        compat.supports_openai_grammar_tools,
    )?;
    let deferred_tools_mode = if compat.supports_additional_tools {
        Some("additional-tools".to_string())
    } else if compat.supports_tool_search {
        Some("tool-search".to_string())
    } else {
        None
    };
    let (immediate_tools, deferred_tools) =
        split_deferred_tools(context, deferred_tools_mode.is_some());
    let tool_options = ConvertResponsesToolsOptions {
        strict: None,
        supports_strict_mode: compat.supports_strict_mode,
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools,
    };
    let messages = convert_responses_messages_checked(
        model,
        context,
        &["openai", "openai-codex", "opencode"],
        &ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: grammar_properties.clone(),
            deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
            deferred_tools_mode,
            tool_options: Some(tool_options.clone()),
            ..Default::default()
        },
    )?;

    let cache_retention = resolve_cache_retention(
        options.base.cache_retention.as_deref(),
        options.base.base.env.as_ref(),
    );
    let disable_implicit_cache =
        cache_retention == "none" && compat.supports_explicit_prompt_cache_mode;

    let mut params = json!({
        "model": model.id,
        "input": messages,
        "stream": true,
        "store": false,
    });

    // The TypeScript adaptor uses `undefined` for optional cache fields. They
    // therefore disappear during JSON serialization; sending JSON nulls is a
    // different wire contract and is rejected by some Responses-compatible
    // gateways.
    if cache_retention != "none" {
        if let Some(key) = clamp_openai_prompt_cache_key(options.base.session_id.as_deref()) {
            params["prompt_cache_key"] = Value::String(key);
        }
        if let Some(retention) = get_prompt_cache_retention(&compat, &cache_retention) {
            params["prompt_cache_retention"] = Value::String(retention.to_string());
        }
    }
    if disable_implicit_cache {
        params["prompt_cache_options"] = json!({ "mode": "explicit" });
    }

    if let Some(max_tokens) = options.base.max_tokens {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.base.temperature {
        params["temperature"] = json!(temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        params["service_tier"] = json!(service_tier);
    }
    if !immediate_tools.is_empty() {
        let tools = convert_responses_tools(&immediate_tools, &tool_options);
        // null-remove: only set when non-empty
        params["tools"] = json!(tools?);
    }
    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = tool_choice.clone();
    }

    if model.reasoning {
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = options
                .reasoning_effort
                .clone()
                .or_else(|| Some("medium".to_string()));
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| {
                    m.get(&ModelThinkingLevel::from_effort_str(
                        effort.as_deref().unwrap_or("medium"),
                    ))
                })
                .cloned()
                .flatten()
                .unwrap_or_else(|| effort.unwrap_or_else(|| "medium".to_string()));
            params["reasoning"] = json!({
                "effort": effort,
                "summary": options.reasoning_summary.clone().unwrap_or_else(|| "auto".to_string()),
            });
            params["include"] = json!(["reasoning.encrypted_content"]);
        } else if model.provider != "github-copilot"
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::Off))
                .is_some()
        {
            let off_entry = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&ModelThinkingLevel::Off))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_string());
            params["reasoning"] = json!({ "effort": off_entry });
        }
        if model.provider == "xai" {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    // Sampling params override named fields last.
    if let Some(sp) = &options.base.sampling_params {
        if let Some(obj) = sp.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
    }

    Ok(params)
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

/// Streams a request against the OpenAI Responses API.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &OpenAIResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let Some(sender) = stream.sender() else {
        return stream;
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return immediate_error_stream(&model, "Request was aborted", true);
    }
    let api_key =
        match get_client_api_key(&model.provider, api_key, options.base.base.headers.as_ref()) {
            Ok(k) => k,
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                super::anthropic_messages::set_error_message(&mut message, err);
                return crate::event_stream::create_error_stream(
                    &model.api,
                    &model.provider,
                    &model.id,
                    message.error_message().unwrap_or("").to_string(),
                );
            }
        };
    let base_url = base_url.to_string();

    let handle = tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
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
        let compat = OpenAIResponsesCompat::get(&model);
        let grammar_properties =
            match super::constrained_sampling::create_grammar_tool_input_properties(
                &context.tools,
                compat.supports_openai_grammar_tools,
            ) {
                Ok(properties) => properties,
                Err(error) => {
                    let mut message = new_output(&model);
                    message.set_stop_reason(StopReason::Error);
                    super::anthropic_messages::set_error_message(&mut message, error);
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                    return;
                }
            };

        let endpoint = format!("{base_url}/responses");
        let headers = build_request_headers(
            &model,
            &context,
            options.base.base.headers.as_ref(),
            options.base.session_id.as_deref(),
            &compat,
        );
        let mut request = client
            .post(&endpoint)
            .header("content-type", "application/json");
        if !has_header_name(&headers, "authorization") {
            request = request.bearer_auth(&api_key);
        }
        for (name, value) in headers {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        request = request.json(&params);

        let response = match abortable(request.send(), options.base.abort_signal.clone()).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                let message = terminal_error_message(
                    &model,
                    format!("Request failed: {}", format_reqwest_error(&err)),
                    false,
                );
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
        let body_result = match abortable_with_timeout(
            response.bytes(),
            options.base.abort_signal.clone(),
            options.base.base.timeout_ms,
        )
        .await
        {
            Ok(result) => result,
            Err(AbortTimeoutError::TimedOut(timeout_ms)) => {
                let message = terminal_error_message(
                    &model,
                    format!("Request timed out after {timeout_ms}ms"),
                    false,
                );
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
            Err(AbortTimeoutError::Aborted) => {
                let message = terminal_error_message(&model, "Request was aborted", true);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let body = match body_result {
            Ok(body) => body,
            Err(err) => {
                let message = terminal_error_message(
                    &model,
                    format!("Request body failed: {}", format_reqwest_error(&err)),
                    false,
                );
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
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
            let detail = extract_openai_responses_error(&body_text);
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            super::anthropic_messages::set_error_message(
                &mut message,
                format!("OpenAI API error ({}): {}", status.as_u16(), detail),
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
        let mut output = new_output(&model);
        let proc_options = ProcessResponsesOptions {
            service_tier: options.service_tier.clone(),
            grammar_tool_input_properties: grammar_properties,
        };
        match process_responses_stream(
            &events,
            &mut output,
            &mut |event| pusher.push(event),
            &model,
            &proc_options,
        ) {
            Ok(()) => {
                if signal_aborted(options.base.abort_signal.as_ref()) {
                    let message = terminal_error_message(&model, "Request was aborted", true);
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Aborted,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                    return;
                }
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

/// Pull the OpenAI error message from an error response body.
pub fn extract_openai_responses_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return crate::utils::error_body::truncate_provider_error_text(msg);
        }
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return crate::utils::error_body::truncate_provider_error_text(msg);
        }
        if let Some(detail) = value.get("detail").and_then(|d| d.as_str()) {
            return crate::utils::error_body::truncate_provider_error_text(detail);
        }
    }
    crate::utils::error_body::truncate_provider_error_text(body.trim())
}

/// Simple stream: resolves reasoning effort and forwards (upstream
/// `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let compat = OpenAIResponsesCompat::get(model);
    let _ = compat;
    let reasoning_effort = options.reasoning.and_then(|r| {
        let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(r));
        if clamped == ModelThinkingLevel::Off {
            None
        } else {
            Some(clamped.as_str().to_string())
        }
    });
    let go = OpenAIResponsesOptions {
        base: options.base.clone(),
        reasoning_effort,
        reasoning_summary: None,
        service_tier: None,
        tool_choice: options.tool_choice.as_ref().map(|t| match t {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
        }),
    };
    stream(model, context, client, base_url, api_key, &go)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Model, ModelInput};
    use crate::types::*;

    fn model(id: &str) -> Model {
        let mut m = Model::new(id, id, "openai-responses", "openai");
        m.base_url = "https://api.openai.com/v1".to_string();
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
    fn build_params_shape() {
        let m = model("gpt-5");
        let params = build_params(&m, &ctx(), &OpenAIResponsesOptions::default()).unwrap();
        assert_eq!(params["model"], "gpt-5");
        assert_eq!(params["stream"], true);
        assert_eq!(params["store"], false);
        assert_eq!(params["input"][0]["role"], "developer");
        assert_eq!(params["input"][0]["content"], "You are helpful.");
        assert_eq!(params["tools"][0]["type"], "function");
        assert_eq!(params["tools"][0]["name"], "bash");
        // default reasoning for reasoning model with no explicit effort
        assert!(params.get("reasoning").is_none());
    }

    #[test]
    fn build_params_reasoning_effort() {
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions {
            base: StreamOptions::default(),
            reasoning_effort: Some("high".to_string()),
            reasoning_summary: Some("detailed".to_string()),
            service_tier: None,
            tool_choice: None,
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(params["reasoning"]["effort"], "high");
        assert_eq!(params["reasoning"]["summary"], "detailed");
        assert_eq!(params["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn xai_responses_requests_encrypted_reasoning_and_session_affinity() {
        let mut m = model("grok-4.6");
        m.provider = "xai".to_string();
        m.base_url = "https://api.x.ai/v1".to_string();
        let options = OpenAIResponsesOptions {
            base: StreamOptions {
                session_id: Some("xai-session".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let params = build_params(&m, &ctx(), &options).unwrap();
        assert_eq!(params["model"], "grok-4.6");
        assert_eq!(params["include"], json!(["reasoning.encrypted_content"]));
        assert!(params.get("reasoning").is_none());
        assert_eq!(params["prompt_cache_key"], "xai-session");
        let compat = OpenAIResponsesCompat::get(&m);
        let headers = build_request_headers(&m, &ctx(), None, Some("xai-session"), &compat);
        assert_eq!(
            headers.get("session_id"),
            Some(&Some("xai-session".to_string()))
        );
        assert_eq!(
            headers.get("x-client-request-id"),
            Some(&Some("xai-session".to_string()))
        );
    }

    #[test]
    fn build_params_max_tokens_floored() {
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions {
            base: StreamOptions {
                max_tokens: Some(5),
                ..Default::default()
            },
            reasoning_effort: None,
            reasoning_summary: None,
            service_tier: None,
            tool_choice: None,
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(
            params["max_output_tokens"],
            OPENAI_RESPONSES_MIN_OUTPUT_TOKENS
        );
    }

    #[test]
    fn prompt_cache_key_clamped() {
        let long_key = "k".repeat(100);
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions {
            base: StreamOptions {
                session_id: Some(long_key.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(params["prompt_cache_key"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn optional_prompt_cache_fields_are_omitted_when_not_configured() {
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions {
            base: StreamOptions {
                cache_retention: Some("short".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert!(params.get("prompt_cache_key").is_none());
        assert!(params.get("prompt_cache_retention").is_none());
        assert!(params.get("prompt_cache_options").is_none());

        let mut explicit_cache_model = m.clone();
        explicit_cache_model.compat = Some(json!({
            "supportsExplicitPromptCacheMode": true,
        }));
        let opts = OpenAIResponsesOptions {
            base: StreamOptions {
                cache_retention: Some("none".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let params = build_params(&explicit_cache_model, &ctx(), &opts).unwrap();
        assert!(params.get("prompt_cache_key").is_none());
        assert!(params.get("prompt_cache_retention").is_none());
        assert_eq!(
            params["prompt_cache_options"],
            json!({ "mode": "explicit" })
        );
    }

    #[test]
    fn provider_env_controls_cache_retention_without_global_env() {
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    env: Some(BTreeMap::from([(
                        "PI_CACHE_RETENTION".to_string(),
                        "long".to_string(),
                    )])),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let params = build_params(&m, &ctx(), &opts).unwrap();
        assert_eq!(params["prompt_cache_retention"], "24h");
    }

    #[test]
    fn client_api_key_handling() {
        assert_eq!(
            get_client_api_key("openai", Some("sk-123"), None).unwrap(),
            "sk-123"
        );
        // Authorization header present -> "unused" placeholder key.
        let mut headers = crate::types::ProviderHeaders::new();
        headers.insert(
            "authorization".to_string(),
            Some("Bearer sk-456".to_string()),
        );
        assert_eq!(
            get_client_api_key("openai", None, Some(&headers)).unwrap(),
            "unused"
        );
        let err = get_client_api_key("openai", None, None).unwrap_err();
        assert!(err.contains("No API key for provider: openai"));
    }

    #[test]
    fn stream_without_key_is_terminal_error() {
        let m = model("gpt-5");
        let opts = OpenAIResponsesOptions::default();
        let s = stream(
            &m,
            &Context::default(),
            reqwest::Client::new(),
            "https://api.openai.com/v1",
            None,
            &opts,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, msg) = rt.block_on(s.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(err.contains("No API key for provider: openai"), "{err}");
    }
}
