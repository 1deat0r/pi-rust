//! OpenAI Completions API — port of `packages/ai/src/api/openai-completions.ts`.
//!
//! The most widely reused adaptor: ~21 of the 39 built-in providers speak
//! `openai-completions` (openai via completions gateways, deepseek, groq,
//! together, fireworks, cerebras, nvidia, moonshotai, qwen variants, zai,
//! xiaomi, ant-ling, baseten, huggingface, openrouter, opencode-go, etc.).
//!
//! Ported surface: compat detection/override, convertMessages (text/image/
//! thinking/tool-call/tool-result content blocks, tool-call-id normalization,
//! cross-model thinking-signature downgrade), convertTools (function tools
//! with strict JSON-schema where supported), buildParams (incl. the
//! per-provider thinking formats), chunk usage parsing, stop-reason mapping,
//! and the streaming event loop (text/thinking/tool-call deltas with
//! streaming JSON for tool arguments). OpenAI grammar custom tools are
//! converted and replayed through the same streaming path, including the
//! provider-specific chat-template reasoning fields.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};

use crate::event_stream::StreamSink;
use crate::model::{calculate_cost, clamp_thinking_level, Model};
use crate::partial_json::parse_streaming_json;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context, DoneReason,
    ErrorReason, ProviderEnv, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions,
    Tool, ToolChoice, Usage,
};
use crate::AssistantMessageEventStream;

use super::constrained_sampling::{
    append_grammar_tool_input_json_delta, create_grammar_tool_input_properties,
    get_grammar_tool_input, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling as shared_resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling as shared_resolve_json_schema_strict_sampling,
    GrammarToolInputJsonBuffer,
};

const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const BASE_RETRY_DELAY_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Compatibility
// ---------------------------------------------------------------------------

/// Resolved OpenAI-completions compatibility settings (upstream
/// `ResolvedOpenAICompletionsCompat`).
#[derive(Debug, Clone)]
pub struct OpenAiCompletionsCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: String, // "max_completion_tokens" | "max_tokens"
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: String, // "openai" | "deepseek" | "zai" | "together" | "ant-ling" | "openrouter" | ...
    pub zai_tool_stream: bool,
    pub supports_thinking_token_budget: bool,
    pub thinking_token_budget_field: Option<String>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub cache_control_format: Option<String>, // "anthropic" | None
    pub send_session_affinity_headers: bool,
    pub deferred_tools_mode: Option<String>,
    pub session_affinity_format: String, // "openai" | "openrouter" | "openai-nosession"
    pub supports_long_cache_retention: bool,
    /// Provider compatibility values resolved into `chat_template_kwargs`.
    pub chat_template_kwargs: BTreeMap<String, Value>,
    /// Provider compatibility values resolved into `chat_template_args`.
    pub chat_template_args: BTreeMap<String, Value>,
}

impl OpenAiCompletionsCompat {
    /// Auto-detect from provider/URL (upstream `detectCompat`).
    pub fn detect(model: &Model) -> Self {
        let provider = &model.provider;
        let base_url = model.base_url.to_lowercase();

        let is_zai = provider == "zai"
            || provider == "zai-coding-cn"
            || base_url.contains("api.z.ai")
            || base_url.contains("open.bigmodel.cn");
        let is_together = provider == "together"
            || base_url.contains("api.together.ai")
            || base_url.contains("api.together.xyz");
        let is_moonshot = provider == "moonshotai"
            || provider == "moonshotai-cn"
            || base_url.contains("api.moonshot.");
        let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
        let is_cloudflare_workers_ai =
            provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
        let is_cloudflare_ai_gateway =
            provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
        let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
        let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
        let is_deepseek = provider == "deepseek" || base_url.contains("deepseek.com");

        let is_non_standard = is_nvidia
            || provider == "cerebras"
            || base_url.contains("cerebras.ai")
            || provider == "xai"
            || base_url.contains("api.x.ai")
            || is_together
            || base_url.contains("chutes.ai")
            || is_deepseek
            || is_zai
            || is_moonshot
            || provider == "opencode"
            || base_url.contains("opencode.ai")
            || is_cloudflare_workers_ai
            || is_cloudflare_ai_gateway
            || is_ant_ling;

        let use_max_tokens = base_url.contains("chutes.ai")
            || is_deepseek
            || is_moonshot
            || is_cloudflare_ai_gateway
            || is_together
            || is_nvidia
            || is_ant_ling
            || is_zai;

        let is_grok = provider == "xai" || base_url.contains("api.x.ai");
        let is_openrouter_developer_role_model = is_openrouter
            && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/")
        {
            Some("anthropic".to_string())
        } else {
            None
        };

        let thinking_format = if is_deepseek {
            "deepseek"
        } else if is_zai {
            "zai"
        } else if is_together {
            "together"
        } else if is_ant_ling {
            "ant-ling"
        } else if is_openrouter {
            "openrouter"
        } else {
            "openai"
        };

        Self {
            supports_store: !is_non_standard,
            supports_developer_role: is_openrouter_developer_role_model
                || (!is_non_standard && !is_openrouter),
            supports_reasoning_effort: !is_grok
                && !is_zai
                && !is_moonshot
                && !is_together
                && !is_cloudflare_ai_gateway
                && !is_nvidia
                && !is_ant_ling,
            supports_usage_in_streaming: true,
            supports_finish_reason: true,
            max_tokens_field: if use_max_tokens {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }
            .to_string(),
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant_messages: is_deepseek,
            thinking_format: thinking_format.to_string(),
            zai_tool_stream: false,
            supports_thinking_token_budget: false,
            thinking_token_budget_field: None,
            supports_strict_mode: !is_moonshot
                && !is_together
                && !is_cloudflare_ai_gateway
                && !is_nvidia,
            supports_openai_grammar_tools: false,
            cache_control_format,
            send_session_affinity_headers: false,
            deferred_tools_mode: None,
            session_affinity_format: if is_openrouter {
                "openrouter"
            } else {
                "openai"
            }
            .to_string(),
            supports_long_cache_retention: !(is_together
                || is_cloudflare_workers_ai
                || is_cloudflare_ai_gateway
                || is_nvidia
                || is_ant_ling),
            chat_template_kwargs: BTreeMap::new(),
            chat_template_args: BTreeMap::new(),
        }
    }

    /// Get resolved compatibility for a model (upstream `getCompat`):
    /// auto-detect then override with explicit model.compat.
    pub fn get(model: &Model) -> Self {
        let detected = Self::detect(model);
        let Some(compat) = &model.compat else {
            return detected;
        };
        let get_bool = |k: &str| compat.get(k).and_then(|v| v.as_bool());
        let get_str = |k: &str| {
            compat
                .get(k)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let get_str_opt = |k: &str| compat.get(k).cloned();
        let get_object = |k: &str| {
            compat
                .get(k)
                .and_then(Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default()
        };
        let cache_control_format_override = match get_str_opt("cacheControlFormat") {
            Some(Value::String(s)) => Some(s),
            Some(Value::Null) => None,
            _ => detected.cache_control_format.clone(),
        };

        Self {
            supports_store: get_bool("supportsStore").unwrap_or(detected.supports_store),
            supports_developer_role: get_bool("supportsDeveloperRole")
                .unwrap_or(detected.supports_developer_role),
            supports_reasoning_effort: get_bool("supportsReasoningEffort")
                .unwrap_or(detected.supports_reasoning_effort),
            supports_usage_in_streaming: get_bool("supportsUsageInStreaming")
                .unwrap_or(detected.supports_usage_in_streaming),
            supports_finish_reason: get_bool("supportsFinishReason")
                .unwrap_or(detected.supports_finish_reason),
            max_tokens_field: get_str("maxTokensField").unwrap_or(detected.max_tokens_field),
            requires_tool_result_name: get_bool("requiresToolResultName")
                .unwrap_or(detected.requires_tool_result_name),
            requires_assistant_after_tool_result: get_bool("requiresAssistantAfterToolResult")
                .unwrap_or(detected.requires_assistant_after_tool_result),
            requires_thinking_as_text: get_bool("requiresThinkingAsText")
                .unwrap_or(detected.requires_thinking_as_text),
            requires_reasoning_content_on_assistant_messages: get_bool(
                "requiresReasoningContentOnAssistantMessages",
            )
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
            thinking_format: get_str("thinkingFormat").unwrap_or(detected.thinking_format),
            zai_tool_stream: get_bool("zaiToolStream").unwrap_or(detected.zai_tool_stream),
            supports_thinking_token_budget: get_bool("supportsThinkingTokenBudget")
                .unwrap_or(detected.supports_thinking_token_budget),
            thinking_token_budget_field: get_str("thinkingTokenBudgetField")
                .or(detected.thinking_token_budget_field),
            supports_strict_mode: get_bool("supportsStrictMode")
                .unwrap_or(detected.supports_strict_mode),
            supports_openai_grammar_tools: get_bool("supportsOpenAIGrammarTools")
                .unwrap_or(detected.supports_openai_grammar_tools),
            cache_control_format: cache_control_format_override,
            send_session_affinity_headers: get_bool("sendSessionAffinityHeaders")
                .unwrap_or(detected.send_session_affinity_headers),
            deferred_tools_mode: get_str("deferredToolsMode").or(detected.deferred_tools_mode),
            session_affinity_format: get_str("sessionAffinityFormat")
                .unwrap_or(detected.session_affinity_format),
            supports_long_cache_retention: get_bool("supportsLongCacheRetention")
                .unwrap_or(detected.supports_long_cache_retention),
            chat_template_kwargs: get_object("chatTemplateKwargs"),
            chat_template_args: get_object("chatTemplateArgs"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else { return false };
    let expected = name.to_lowercase();
    headers.iter().any(|(k, v)| {
        k.to_lowercase() == expected && v.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
    })
}

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
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
    compat: &OpenAiCompletionsCompat,
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
        if compat.send_session_affinity_headers {
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
                replace_header(
                    &mut headers,
                    "x-session-affinity",
                    Some(session_id.to_string()),
                );
            }
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

fn has_tool_history(messages: &[crate::types::Message]) -> bool {
    for msg in messages {
        match msg {
            crate::types::Message::ToolResult(_) => return true,
            crate::types::Message::Assistant(a)
                if a.content()
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. })) =>
            {
                return true
            }
            _ => {}
        }
    }
    false
}

/// Return deferred tool names in transcript order, matching the upstream
/// `Set` insertion order while ignoring duplicate load notifications.
fn get_deferred_tool_names(messages: &[crate::types::Message]) -> Vec<String> {
    let mut names = Vec::new();
    for message in messages {
        let crate::types::Message::ToolResult(result) = message else {
            continue;
        };
        let crate::types::ToolResultMessage::ToolResult {
            added_tool_names: Some(added_tool_names),
            ..
        } = result
        else {
            continue;
        };
        for name in added_tool_names {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// Resolve deferred tool names against the current context in transcript order,
/// omitting stale references that no longer have a current definition.
fn get_tools_by_name(context: &Context, names: &[String]) -> Vec<Tool> {
    names
        .iter()
        .filter_map(|name| context.tools.iter().find(|tool| tool.name == *name))
        .cloned()
        .collect()
}

pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(env) = env {
        if let Some(v) = env.get(name) {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn resolve_cache_retention(
    cache_retention: Option<&CacheRetention>,
    env: Option<&ProviderEnv>,
) -> String {
    if let Some(retention) = cache_retention {
        return retention.clone();
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return "long".to_string();
    }
    "short".to_string()
}

/// Deterministic short hash (upstream `shortHash`).
pub fn short_hash(text: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for ch in text.chars() {
        let c = ch as u32;
        h1 = (h1 ^ c).wrapping_mul(2654435761);
        h2 = (h2 ^ c).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{:x}{:x}", h2, h1)
}

/// Normalize a tool-call id (upstream `normalizeToolCallId` in convertMessages).
fn normalize_tool_call_id(id: &str, provider: &str) -> String {
    if id.contains('|') {
        let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
        let sanitize = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        let call_id = sanitize(call_id);
        let item_id = sanitize(item_id);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if combined.len() <= 40 {
            return combined;
        }
        let hash = short_hash(id);
        let hash = &hash[..hash.len().min(8)];
        let prefix_len = (40usize.saturating_sub(hash.len() + 1)).max(1);
        let prefix: String = call_id.chars().take(prefix_len).collect();
        return format!("{prefix}_{hash}");
    }
    if provider == "openai" && id.len() > 40 {
        return id.chars().take(40).collect();
    }
    id.to_string()
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Compatibility wrapper for callers that only need ordinary function-tool
/// replay. It remains fallible so required grammar/strict constraints are
/// never silently ignored.
pub fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &OpenAiCompletionsCompat,
) -> Result<Vec<Value>, String> {
    let grammar_properties =
        create_grammar_tool_input_properties(&context.tools, compat.supports_openai_grammar_tools)?;
    convert_messages_with_grammar(model, context, compat, &grammar_properties)
}

/// Port of `convertMessages` in openai-completions.ts. Produces the
/// `messages` array for the Chat Completions request.
pub fn convert_messages_with_grammar(
    model: &Model,
    context: &Context,
    compat: &OpenAiCompletionsCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Vec<Value>, String> {
    let mut params: Vec<Value> = Vec::new();
    let _provider = model.provider.clone();

    // Transform messages: downgrade unsupported images + normalize tool ids.
    let transformed = transform_messages(model, &context.messages);

    if let Some(system_prompt) = &context.system_prompt {
        let use_developer_role = model.reasoning && compat.supports_developer_role;
        let role = if use_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({ "role": role, "content": system_prompt }));
    }

    let mut last_role: Option<&str> = None;
    let mut i = 0usize;

    while i < transformed.len() {
        let msg = &transformed[i];
        match msg {
            crate::types::Message::User(user) => {
                if compat.requires_assistant_after_tool_result && last_role == Some("toolResult") {
                    params.push(json!({ "role": "assistant", "content": "I have processed the tool results." }));
                }
                match user.content() {
                    crate::types::UserContentBody::String(text) => {
                        params.push(json!({ "role": "user", "content": text }));
                    }
                    crate::types::UserContentBody::Blocks(blocks) => {
                        let mut content: Vec<Value> = Vec::new();
                        for block in blocks {
                            match block {
                                ContentBlock::Text { text, .. } => {
                                    content.push(json!({ "type": "text", "text": text }));
                                }
                                ContentBlock::Image {
                                    data, mime_type, ..
                                } => {
                                    content.push(json!({
                                        "type": "image_url",
                                        "image_url": { "url": format!("data:{mime_type};base64,{data}") }
                                    }));
                                }
                                _ => {}
                            }
                        }
                        if !content.is_empty() {
                            params.push(json!({ "role": "user", "content": content }));
                        }
                    }
                }
                last_role = Some("user");
            }
            crate::types::Message::Assistant(a) => {
                let mut assistant_msg = serde_json::Map::new();
                assistant_msg.insert("role".into(), json!("assistant"));
                let assistant_content: Option<Value> =
                    if compat.requires_assistant_after_tool_result {
                        Some(Value::String(String::new()))
                    } else {
                        None
                    };
                if let Some(c) = assistant_content {
                    assistant_msg.insert("content".into(), c);
                }

                let blocks = a.content();
                let assistant_text_parts: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .collect();
                let assistant_text = assistant_text_parts.join("");

                let thinking_blocks: Vec<&ContentBlock> = blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
                    .collect();
                let tool_calls: Vec<&ContentBlock> = blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                    .collect();
                let preserved_reasoning_details =
                    thinking_blocks.iter().find_map(|block| match block {
                        ContentBlock::Thinking {
                            thinking_signature: Some(signature),
                            ..
                        } => parse_openai_reasoning_details(signature),
                        _ => None,
                    });

                let non_empty_thinking: Vec<&ContentBlock> = thinking_blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty()))
                    .cloned()
                    .collect();

                if !non_empty_thinking.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty_thinking
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut text_parts: Vec<Value> = Vec::new();
                        text_parts.push(json!({ "type": "text", "text": thinking_text }));
                        for part in &assistant_text_parts {
                            text_parts.push(json!({ "type": "text", "text": part }));
                        }
                        assistant_msg.insert("content".into(), json!(text_parts));
                    } else {
                        if !assistant_text.is_empty() {
                            assistant_msg.insert("content".into(), json!(assistant_text));
                        }
                        if preserved_reasoning_details.is_none() {
                            // Reasoning signature replay: use the first
                            // thinking block's signature as the raw reasoning
                            // field when structured details are unavailable.
                            let signature =
                                non_empty_thinking[0].as_thinking().and_then(|t| match t {
                                    ContentBlock::Thinking {
                                        thinking_signature, ..
                                    } => thinking_signature.clone(),
                                    _ => None,
                                });
                            let signature = signature.as_deref();
                            if let Some(sig) = signature {
                                let sig = if model.provider == "opencode-go" && sig == "reasoning" {
                                    "reasoning_content"
                                } else {
                                    sig
                                };
                                if is_openai_completions_reasoning_field(sig) {
                                    let content = non_empty_thinking
                                        .iter()
                                        .filter_map(|b| match b {
                                            ContentBlock::Thinking { thinking, .. } => {
                                                Some(thinking.clone())
                                            }
                                            _ => None,
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    assistant_msg.insert(sig.into(), json!(content));
                                }
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg.insert("content".into(), json!(assistant_text));
                }

                if !tool_calls.is_empty() {
                    let mut converted = Vec::with_capacity(tool_calls.len());
                    for block in &tool_calls {
                        let ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } = block
                        else {
                            continue;
                        };
                        if let Some(input_property) = grammar_tool_input_properties.get(name) {
                            let input = get_grammar_tool_input(name, arguments, input_property)?;
                            converted.push(json!({
                                "id": id,
                                "type": "custom",
                                "custom": { "name": name, "input": input },
                            }));
                        } else {
                            converted.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into())
                                }
                            }));
                        }
                    }
                    assistant_msg.insert("tool_calls".into(), json!(converted));
                }
                if let Some(details) = preserved_reasoning_details {
                    assistant_msg.insert("reasoning_details".into(), Value::Array(details));
                }

                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_msg.contains_key("reasoning_content")
                {
                    assistant_msg.insert("reasoning_content".into(), json!(""));
                }

                // Skip assistant messages with no content and no tool calls.
                let has_content = match assistant_msg.get("content") {
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Array(a)) => !a.is_empty(),
                    _ => false,
                };
                if !has_content && !assistant_msg.contains_key("tool_calls") {
                    last_role = Some("assistant");
                    i += 1;
                    continue;
                }
                params.push(Value::Object(assistant_msg));
                last_role = Some("assistant");
            }
            crate::types::Message::ToolResult(_tr) => {
                // Group consecutive tool-result messages (upstream advances j).
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut deferred_tool_names = Vec::new();
                let mut j = i;
                while j < transformed.len() {
                    if let crate::types::Message::ToolResult(tr) = &transformed[j] {
                        let text_result = tr
                            .content()
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let has_images = tr
                            .content()
                            .iter()
                            .any(|b| matches!(b, ContentBlock::Image { .. }));
                        let has_text = !text_result.is_empty();
                        let tool_result_text = if has_text {
                            text_result
                        } else if has_images {
                            "(see attached image)".to_string()
                        } else {
                            "(no tool output)".to_string()
                        };
                        let mut tool_msg = serde_json::Map::new();
                        tool_msg.insert("role".into(), json!("tool"));
                        tool_msg.insert("content".into(), json!(tool_result_text));
                        tool_msg.insert("tool_call_id".into(), json!(tr.tool_call_id()));
                        if compat.requires_tool_result_name {
                            tool_msg.insert("name".into(), json!(tr.tool_name()));
                        }
                        params.push(Value::Object(tool_msg));

                        if compat.deferred_tools_mode.as_deref() == Some("kimi") {
                            if let crate::types::ToolResultMessage::ToolResult {
                                added_tool_names: Some(added_tool_names),
                                ..
                            } = tr
                            {
                                for name in added_tool_names {
                                    if !deferred_tool_names.iter().any(|existing| existing == name)
                                    {
                                        deferred_tool_names.push(name.clone());
                                    }
                                }
                            }
                        }

                        if has_images && model.input.contains(&crate::model::ModelInput::Image) {
                            for block in tr.content() {
                                if let ContentBlock::Image {
                                    data, mime_type, ..
                                } = block
                                {
                                    image_blocks.push(json!({
                                        "type": "image_url",
                                        "image_url": { "url": format!("data:{mime_type};base64,{data}") }
                                    }));
                                }
                            }
                        }
                        j += 1;
                    } else {
                        break;
                    }
                }

                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({ "role": "assistant", "content": "I have processed the tool results." }));
                    }
                    let mut user_content: Vec<Value> = vec![
                        json!({ "type": "text", "text": "Attached image(s) from tool result:" }),
                    ];
                    user_content.extend(image_blocks);
                    params.push(json!({ "role": "user", "content": user_content }));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }

                if !deferred_tool_names.is_empty() {
                    let deferred_tools = get_tools_by_name(context, &deferred_tool_names);
                    if !deferred_tools.is_empty() {
                        // Kimi accepts loaded tool definitions in a system
                        // message without a content field. They must follow
                        // the corresponding tool results and are not sent in
                        // the ordinary top-level `tools` array.
                        params.push(json!({
                            "role": "system",
                            "tools": convert_tools(&deferred_tools, compat)?,
                        }));
                    }
                }
                // Advance past the grouped tool results (the while loop adds 1).
                i = j.saturating_sub(1);
            }
        }
        i += 1;
    }

    Ok(params)
}

// ---------------------------------------------------------------------------
// transformMessages (port of api/transform-messages.ts)
// ---------------------------------------------------------------------------

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

fn replace_images_with_placeholder(
    content: &[ContentBlock],
    placeholder: &str,
) -> Vec<ContentBlock> {
    let mut result: Vec<ContentBlock> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            ContentBlock::Image { .. } => {
                if !previous_was_placeholder {
                    result.push(ContentBlock::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            ContentBlock::Text {
                text,
                text_signature,
            } => {
                result.push(ContentBlock::Text {
                    text: text.clone(),
                    text_signature: text_signature.clone(),
                });
                previous_was_placeholder = text == placeholder;
            }
            other => result.push(other.clone()),
        }
    }
    result
}

/// Downgrade unsupported images to placeholder text (upstream
/// `downgradeUnsupportedImages`).
fn downgrade_unsupported_images(
    messages: &[crate::types::Message],
    model: &Model,
) -> Vec<crate::types::Message> {
    if model.input.contains(&crate::model::ModelInput::Image) {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|msg| match msg {
            crate::types::Message::User(msg) => {
                let content = match msg.content() {
                    crate::types::UserContentBody::String(s) => {
                        crate::types::UserContentBody::String(s.clone())
                    }
                    crate::types::UserContentBody::Blocks(blocks) => {
                        crate::types::UserContentBody::Blocks(replace_images_with_placeholder(
                            blocks,
                            NON_VISION_USER_IMAGE_PLACEHOLDER,
                        ))
                    }
                };
                crate::types::Message::User(crate::types::UserContent::RoleUser {
                    content,
                    timestamp: msg.timestamp(),
                })
            }
            crate::types::Message::ToolResult(tr) => {
                let mut cloned = tr.clone();
                cloned.set_content(replace_images_with_placeholder(
                    tr.content(),
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                ));
                crate::types::Message::ToolResult(cloned)
            }
            _ => msg.clone(),
        })
        .collect()
}

fn is_same_model(assistant: &AssistantMessage, model: &Model) -> bool {
    assistant.provider() == Some(model.provider.as_str())
        && assistant.api() == Some(model.api.as_str())
        && assistant.model() == Some(model.id.as_str())
}

fn insert_synthetic_tool_results(
    result: &mut Vec<crate::types::Message>,
    pending_tool_calls: &mut Vec<(String, String)>,
    existing_tool_result_ids: &mut BTreeSet<String>,
) {
    for (tool_call_id, tool_name) in pending_tool_calls.drain(..) {
        if !existing_tool_result_ids.contains(&tool_call_id) {
            result.push(crate::types::Message::ToolResult(
                crate::types::ToolResultMessage::new(
                    tool_call_id,
                    tool_name,
                    vec![ContentBlock::text("No result provided")],
                    true,
                ),
            ));
        }
    }
    existing_tool_result_ids.clear();
}

/// Port of `transformMessages`: cross-model thinking-signature downgrade,
/// redacted-thinking dropping, tool-call-id normalization.
pub fn transform_messages(
    model: &Model,
    messages: &[crate::types::Message],
) -> Vec<crate::types::Message> {
    let mut tool_call_id_map: BTreeMap<String, String> = BTreeMap::new();
    let image_aware = downgrade_unsupported_images(messages, model);
    let mut transformed: Vec<crate::types::Message> = Vec::new();

    // First pass.
    for msg in &image_aware {
        match msg {
            crate::types::Message::User(_) => transformed.push(msg.clone()),
            crate::types::Message::ToolResult(tr) => {
                let normalized = tool_call_id_map
                    .get(tr.tool_call_id())
                    .cloned()
                    .filter(|n| n != tr.tool_call_id());
                if let Some(normalized) = normalized {
                    let mut cloned = tr.clone();
                    cloned.set_tool_call_id(normalized);
                    transformed.push(crate::types::Message::ToolResult(cloned));
                } else {
                    transformed.push(msg.clone());
                }
            }
            crate::types::Message::Assistant(a) => {
                let same_model = is_same_model(a, model);
                let mut new_content: Vec<ContentBlock> = Vec::new();
                for block in a.content() {
                    match block {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if matches!(redacted, Some(true)) {
                                if same_model {
                                    new_content.push(block.clone());
                                }
                                // redacted thinking dropped cross-model
                            } else if same_model
                                && thinking_signature
                                    .as_ref()
                                    .is_some_and(|signature| !signature.is_empty())
                            {
                                new_content.push(block.clone());
                            } else if thinking.trim().is_empty() {
                                // skip empty
                            } else if same_model {
                                new_content.push(block.clone());
                            } else {
                                new_content.push(ContentBlock::text(thinking));
                            }
                        }
                        ContentBlock::Text { .. } => new_content.push(block.clone()),
                        ContentBlock::ToolCall { id, .. } => {
                            let mut tc = block.clone();
                            if !same_model {
                                match &tc {
                                    ContentBlock::ToolCall {
                                        thought_signature, ..
                                    } if thought_signature.is_some() => {
                                        tc.clear_thought_signature();
                                    }
                                    _ => {}
                                }
                            }
                            if !same_model {
                                let normalized = normalize_tool_call_id(id, &model.provider);
                                if normalized != *id {
                                    tool_call_id_map.insert(id.clone(), normalized.clone());
                                    tc.set_tool_call_id(normalized);
                                }
                            }
                            new_content.push(tc);
                        }
                        ContentBlock::Image { .. } => new_content.push(block.clone()),
                    }
                }
                let mut cloned = a.clone();
                cloned.set_content(new_content);
                transformed.push(crate::types::Message::Assistant(cloned));
            }
        }
    }

    // Second pass (upstream): preserve the assistant/tool-call sequence by
    // inserting an error tool result whenever a tool call is interrupted by a
    // new assistant/user turn or by the end of the conversation. Providers
    // reject an assistant tool call without a following tool result, and the
    // synthetic result is deliberately marked as an error so the missing
    // execution is not mistaken for a successful tool invocation.
    let mut result = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new();
    let mut existing_tool_result_ids = BTreeSet::new();

    for msg in transformed {
        match &msg {
            crate::types::Message::Assistant(assistant) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                // Upstream drops incomplete assistant turns so partial
                // reasoning/tool-call output is not replayed into a later
                // request as an invalid conversation item.
                if matches!(
                    assistant.stop_reason(),
                    Some(StopReason::Error | StopReason::Aborted)
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
                result.push(msg);
            }
            crate::types::Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id().to_string());
                result.push(msg);
            }
            crate::types::Message::User(_) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(msg);
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

// ---------------------------------------------------------------------------
// Tool conversion
// ---------------------------------------------------------------------------

fn is_openai_completions_reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning_content" | "reasoning" | "reasoning_text")
}

/// Port of `convertTools`, including OpenAI custom grammar tools and strict
/// JSON-schema resolution. Unsupported required constraints are returned to the
/// caller instead of silently dropping the tool.
pub fn convert_tools(
    tools: &[Tool],
    compat: &OpenAiCompletionsCompat,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for tool in tools {
        if let Some(grammar) =
            shared_resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)?
        {
            out.push(json!({
                "type": "custom",
                "custom": {
                    "name": tool.name,
                    "description": tool.description,
                    "format": {
                        "type": "grammar",
                        "grammar": {
                            "syntax": grammar.format,
                            "definition": grammar.definition,
                        }
                    }
                }
            }));
            continue;
        }

        let strict = shared_resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)?;
        let parameters = get_json_schema_tool_parameters(tool, strict)?;
        let mut function = serde_json::Map::new();
        function.insert("name".into(), json!(tool.name));
        function.insert("description".into(), json!(tool.description));
        function.insert("parameters".into(), parameters);
        if compat.supports_strict_mode {
            function.insert("strict".into(), json!(strict.unwrap_or(false)));
        }
        out.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    Ok(out)
}

fn get_compat_cache_control(
    compat: &OpenAiCompletionsCompat,
    cache_retention: &str,
) -> Option<Value> {
    if compat.cache_control_format.as_deref() != Some("anthropic") || cache_retention == "none" {
        return None;
    }
    let ttl = if cache_retention == "long" && compat.supports_long_cache_retention {
        Some("1h")
    } else {
        None
    };
    match ttl {
        Some(ttl) => Some(json!({ "type": "ephemeral", "ttl": ttl })),
        None => Some(json!({ "type": "ephemeral" })),
    }
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

/// Port of `buildParams`.
pub fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
    compat: &OpenAiCompletionsCompat,
    cache_retention: &str,
) -> Result<Value, String> {
    let grammar_properties =
        create_grammar_tool_input_properties(&context.tools, compat.supports_openai_grammar_tools)?;
    let mut messages = convert_messages_with_grammar(model, context, compat, &grammar_properties)?;
    let cache_control = get_compat_cache_control(compat, cache_retention);

    let mut params = serde_json::Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("stream".into(), json!(true));

    // prompt_cache_key/prompt_cache_retention for OpenAI + long-retention.
    let base_url_openai = model.base_url.to_lowercase().contains("api.openai.com");
    let openai_cache_key = base_url_openai && cache_retention != "none";
    let long_cache_retention = cache_retention == "long" && compat.supports_long_cache_retention;
    if openai_cache_key || long_cache_retention {
        if let Some(session_id) = options.and_then(|o| o.session_id.as_deref()) {
            if base_url_openai || long_cache_retention {
                let key = clamp_openai_prompt_cache_key(session_id);
                params.insert("prompt_cache_key".into(), json!(key));
            }
        }
        if cache_retention == "long" && compat.supports_long_cache_retention {
            params.insert("prompt_cache_retention".into(), json!("24h"));
        }
    }

    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }

    if let Some(options) = options {
        if let Some(max_tokens) = options.max_tokens {
            if compat.max_tokens_field == "max_tokens" {
                params.insert("max_tokens".into(), json!(max_tokens));
            } else {
                params.insert("max_completion_tokens".into(), json!(max_tokens));
            }
        }
        if let Some(temperature) = options.temperature {
            params.insert("temperature".into(), json!(temperature));
        }
    }

    // Tools: active (non-deferred) tools; empty array when conversation has tool history.
    let deferred_tool_names = if compat.deferred_tools_mode.as_deref() == Some("kimi") {
        get_deferred_tool_names(&context.messages)
    } else {
        Vec::new()
    };
    let deferred_tool_name_set: BTreeSet<&str> =
        deferred_tool_names.iter().map(String::as_str).collect();
    let active_tools: Vec<Tool> = context
        .tools
        .iter()
        .filter(|tool| !deferred_tool_name_set.contains(tool.name.as_str()))
        .cloned()
        .collect();
    if !active_tools.is_empty() {
        params.insert("tools".into(), json!(convert_tools(&active_tools, compat)?));
        if compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    } else if has_tool_history(&context.messages) {
        params.insert("tools".into(), json!([]));
    }

    if let Some(cache_control) = cache_control.as_ref() {
        apply_anthropic_cache_control(&mut messages, params.get_mut("tools"), cache_control);
    }

    // Insert after cache-control processing so the markers are present on the
    // wire payload, matching the upstream OpenRouter/Anthropic compatibility
    // path.
    params.insert("messages".into(), json!(messages));

    // Thinking formats.
    apply_thinking_params(model, options, compat, &mut params);

    Ok(Value::Object(params))
}

fn apply_anthropic_cache_control(
    messages: &mut [Value],
    tools: Option<&mut Value>,
    cache_control: &Value,
) {
    add_cache_control_to_system_prompt(messages, cache_control);
    add_cache_control_to_last_tool(tools, cache_control);
    add_cache_control_to_last_conversation_message(messages, cache_control);
}

fn add_cache_control_to_system_prompt(messages: &mut [Value], cache_control: &Value) {
    for message in messages {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        let is_instruction = matches!(
            object.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        );
        if is_instruction {
            add_cache_control_to_text_content(object, cache_control);
            return;
        }
    }
}

fn add_cache_control_to_last_tool(tools: Option<&mut Value>, cache_control: &Value) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    let Some(tool) = tools.last_mut().and_then(Value::as_object_mut) else {
        return;
    };
    tool.insert("cache_control".to_string(), cache_control.clone());
}

fn add_cache_control_to_last_conversation_message(messages: &mut [Value], cache_control: &Value) {
    for message in messages.iter_mut().rev() {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        let is_conversation = matches!(
            object.get("role").and_then(Value::as_str),
            Some("user" | "assistant" | "tool")
        );
        if is_conversation && add_cache_control_to_text_content(object, cache_control) {
            return;
        }
    }
}

fn add_cache_control_to_text_content(
    message: &mut serde_json::Map<String, Value>,
    cache_control: &Value,
) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content {
        Value::String(text) if !text.is_empty() => {
            let text = text.clone();
            *content = json!([{
                "type": "text",
                "text": text,
                "cache_control": cache_control.clone(),
            }]);
            true
        }
        Value::Array(parts) => {
            for part in parts.iter_mut().rev() {
                let Some(part) = part.as_object_mut() else {
                    continue;
                };
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.insert("cache_control".to_string(), cache_control.clone());
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn default_thinking_budget(level: crate::types::ModelThinkingLevel) -> u64 {
    match level {
        crate::types::ModelThinkingLevel::Minimal => 1024,
        crate::types::ModelThinkingLevel::Low => 2048,
        crate::types::ModelThinkingLevel::Medium => 8192,
        crate::types::ModelThinkingLevel::High
        | crate::types::ModelThinkingLevel::Xhigh
        | crate::types::ModelThinkingLevel::Max => 16384,
        crate::types::ModelThinkingLevel::Off => 0,
    }
}

fn thinking_budget_for_level(
    level: crate::types::ModelThinkingLevel,
    budgets: Option<&crate::types::ThinkingBudgets>,
) -> u64 {
    let custom = budgets.and_then(|budgets| match level {
        crate::types::ModelThinkingLevel::Minimal => budgets.minimal,
        crate::types::ModelThinkingLevel::Low => budgets.low,
        crate::types::ModelThinkingLevel::Medium => budgets.medium,
        crate::types::ModelThinkingLevel::High
        | crate::types::ModelThinkingLevel::Xhigh
        | crate::types::ModelThinkingLevel::Max => budgets.high,
        crate::types::ModelThinkingLevel::Off => None,
    });
    custom.unwrap_or_else(|| default_thinking_budget(level))
}

fn build_params_for_chat_options(
    model: &Model,
    context: &Context,
    options: &OpenAIChatOptions,
    compat: &OpenAiCompletionsCompat,
    cache_retention: &str,
) -> Result<Value, String> {
    let mut base = options.base.clone();
    let mut sampling_params = base
        .sampling_params
        .take()
        .and_then(|params| params.as_object().cloned())
        .unwrap_or_default();

    // `OpenAIChatOptions` is the Rust equivalent of upstream
    // `OpenAICompletionsOptions`; keep its named reasoning controls visible to
    // the shared buildParams implementation without changing StreamOptions.
    if let Some(reasoning_effort) = &options.reasoning_effort {
        sampling_params
            .entry("reasoningEffort".to_string())
            .or_insert_with(|| json!(reasoning_effort));
        if !sampling_params.contains_key("thinkingBudget")
            && !sampling_params.contains_key("thinking_budget")
        {
            let level = thinking_level_from_str(reasoning_effort);
            let ceiling = options
                .base
                .max_tokens
                .unwrap_or(model.max_tokens)
                .saturating_sub(1024);
            let budget =
                thinking_budget_for_level(level, options.thinking_budgets.as_ref()).min(ceiling);
            if budget > 0 {
                sampling_params.insert("thinkingBudget".to_string(), json!(budget));
            }
        }
    }
    if !sampling_params.is_empty() {
        base.sampling_params = Some(Value::Object(sampling_params));
    }

    let mut params = build_params(model, context, Some(&base), compat, cache_retention)?;
    if let Some(tool_choice) = options.tool_choice {
        params["tool_choice"] = serde_json::to_value(tool_choice)
            .map_err(|error| format!("failed to serialize tool choice: {error}"))?;
    }
    Ok(params)
}

fn thinking_level_from_str(s: &str) -> crate::types::ModelThinkingLevel {
    match s {
        "minimal" => crate::types::ModelThinkingLevel::Minimal,
        "low" => crate::types::ModelThinkingLevel::Low,
        "medium" => crate::types::ModelThinkingLevel::Medium,
        "high" => crate::types::ModelThinkingLevel::High,
        "xhigh" => crate::types::ModelThinkingLevel::Xhigh,
        "max" => crate::types::ModelThinkingLevel::Max,
        _ => crate::types::ModelThinkingLevel::Medium,
    }
}

fn simple_reasoning_effort(
    model: &Model,
    reasoning: Option<crate::types::ThinkingLevel>,
) -> Option<String> {
    match reasoning.map(|level| clamp_thinking_level(model, level.into())) {
        Some(level) if level != crate::types::ModelThinkingLevel::Off => {
            Some(level.as_str().to_string())
        }
        _ => None,
    }
}

fn resolve_chat_template_value(
    value: &Value,
    model: &Model,
    reasoning_effort: Option<&str>,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let Value::Object(object) = value else {
        return Some(value.clone());
    };

    if reasoning_effort.is_none()
        && object
            .get("omitWhenOff")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }

    match object.get("$var").and_then(Value::as_str) {
        Some("thinking.enabled") => Some(json!(reasoning_effort.is_some())),
        Some("thinking.budget") => thinking_budget.map(|budget| json!(budget)),
        _ => {
            let level = reasoning_effort
                .map(thinking_level_from_str)
                .unwrap_or(crate::types::ModelThinkingLevel::Off);
            match model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&level))
            {
                Some(Some(mapped)) => Some(json!(mapped)),
                Some(None) => None,
                None => reasoning_effort.map(|effort| json!(effort)),
            }
        }
    }
}

fn build_chat_template_values(
    values: &BTreeMap<String, Value>,
    model: &Model,
    reasoning_effort: Option<&str>,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let mut resolved = serde_json::Map::new();
    for (key, value) in values {
        if let Some(value) =
            resolve_chat_template_value(value, model, reasoning_effort, thinking_budget)
        {
            resolved.insert(key.clone(), value);
        }
    }
    (!resolved.is_empty()).then_some(Value::Object(resolved))
}

fn clamp_openai_prompt_cache_key(session_id: &str) -> String {
    // Upstream clampOpenAIPromptCacheKey truncates; port keeps the string
    // (valid for OpenAI 64-char limit) with a conservative truncation.
    if session_id.len() > 64 {
        session_id.chars().take(64).collect()
    } else {
        session_id.to_string()
    }
}

fn apply_thinking_params(
    model: &Model,
    options: Option<&StreamOptions>,
    compat: &OpenAiCompletionsCompat,
    params: &mut serde_json::Map<String, Value>,
) {
    let reasoning_effort = options
        .and_then(|o| o.sampling_params.as_ref())
        .and_then(|sp| {
            sp.get("reasoningEffort")
                .or_else(|| sp.get("reasoning_effort"))
        })
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let thinking_budget = options
        .and_then(|o| o.sampling_params.as_ref())
        .and_then(|sp| {
            sp.get("thinkingBudget")
                .or_else(|| sp.get("thinking_budget"))
        })
        .and_then(|v| v.as_u64());

    match compat.thinking_format.as_str() {
        "zai" if model.reasoning => {
            if reasoning_effort.is_some() {
                params.insert(
                    "thinking".into(),
                    json!({ "type": "enabled", "clear_thinking": false }),
                );
            } else {
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|map| map.get(&thinking_level_from_str(effort)));
                    match mapped {
                        Some(Some(mapped)) => {
                            params.insert("reasoning_effort".into(), json!(mapped));
                        }
                        Some(None) => {}
                        None => {
                            params.insert("reasoning_effort".into(), json!(effort));
                        }
                    }
                }
            }
        }
        "deepseek" if model.reasoning => {
            if reasoning_effort.is_some() {
                params.insert("thinking".into(), json!({ "type": "enabled" }));
            } else if model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::types::ModelThinkingLevel::Off))
                != Some(&None)
            {
                // off not explicitly mapped to null -> disable
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(&thinking_level_from_str(effort)))
                        .cloned()
                        .flatten();
                    params.insert(
                        "reasoning_effort".into(),
                        json!(mapped.unwrap_or_else(|| effort.clone())),
                    );
                }
            }
        }
        "openrouter" if model.reasoning => {
            if reasoning_effort.is_some() {
                let mapped = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| {
                        m.get(&thinking_level_from_str(
                            reasoning_effort.as_deref().unwrap_or(""),
                        ))
                    })
                    .cloned()
                    .flatten();
                params.insert("reasoning".into(), json!({ "effort": mapped.unwrap_or_else(|| reasoning_effort.clone().unwrap_or_default()) }));
            }
        }
        "together" if model.reasoning => {
            params.insert(
                "reasoning".into(),
                json!({ "enabled": reasoning_effort.is_some() }),
            );
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(&thinking_level_from_str(effort)))
                        .cloned()
                        .flatten();
                    params.insert(
                        "reasoning_effort".into(),
                        json!(mapped.unwrap_or_else(|| effort.clone())),
                    );
                }
            }
        }
        "ant-ling" if model.reasoning => {
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(&thinking_level_from_str(effort)))
                        .cloned()
                        .flatten();
                    params.insert(
                        "reasoning_effort".into(),
                        json!(mapped.unwrap_or_else(|| effort.clone())),
                    );
                }
            }
        }
        "qwen" if model.reasoning => {
            // QwenCloud Token Plan uses the OpenAI-compatible endpoint but
            // its reasoning switch is `enable_thinking`, not the OpenAI
            // `thinking` object. Preserve the upstream contract exactly:
            // send the boolean for every reasoning model, and only add
            // `reasoning_effort` when that catalog entry supports it.
            params.insert("enable_thinking".into(), json!(reasoning_effort.is_some()));
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(&thinking_level_from_str(effort)))
                        .cloned()
                        .flatten();
                    params.insert(
                        "reasoning_effort".into(),
                        json!(mapped.unwrap_or_else(|| effort.clone())),
                    );
                }
            }
        }
        "qwen-chat-template" if model.reasoning => {
            params.insert(
                "chat_template_kwargs".into(),
                json!({
                    "enable_thinking": reasoning_effort.is_some(),
                    "preserve_thinking": true,
                }),
            );
        }
        "chat-template" if model.reasoning => {
            if let Some(values) = build_chat_template_values(
                &compat.chat_template_kwargs,
                model,
                reasoning_effort.as_deref(),
                thinking_budget,
            ) {
                params.insert("chat_template_kwargs".into(), values);
            }
        }
        "baseten" if model.reasoning => {
            if let Some(values) = build_chat_template_values(
                &compat.chat_template_args,
                model,
                reasoning_effort.as_deref(),
                thinking_budget,
            ) {
                params.insert("chat_template_args".into(), values);
            }
            if compat.supports_reasoning_effort {
                let level = reasoning_effort
                    .as_deref()
                    .map(thinking_level_from_str)
                    .unwrap_or(crate::types::ModelThinkingLevel::Off);
                let mapped = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&level));
                let effort = match mapped {
                    Some(Some(mapped)) => Some(mapped.clone()),
                    Some(None) => None,
                    None => reasoning_effort.clone(),
                };
                if let Some(effort) = effort {
                    params.insert("reasoning_effort".into(), json!(effort));
                }
            }
        }
        _ => {
            // Standard openai / others: reasoning_effort passthrough.
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get(&thinking_level_from_str(effort)))
                        .cloned()
                        .flatten();
                    params.insert(
                        "reasoning_effort".into(),
                        json!(mapped.unwrap_or_else(|| effort.clone())),
                    );
                }
            }
            if thinking_budget.is_some() {
                tracing::debug!("thinking token budget deferred in openai-completions");
            }
        }
    }

    // Some OpenAI-compatible local servers expose a provider-specific
    // top-level budget field. The explicit field wins over the vLLM alias,
    // matching upstream `resolveThinkingTokenBudgetField`; the budget itself
    // is populated by `build_params_for_chat_options` from thinkingBudgets.
    if model.reasoning {
        if let Some(field) = resolve_thinking_token_budget_field(compat) {
            if let Some(budget) = thinking_budget.filter(|budget| *budget > 0) {
                params.insert(field.to_string(), json!(budget));
            }
        }
    }
}

fn resolve_thinking_token_budget_field(compat: &OpenAiCompletionsCompat) -> Option<&str> {
    compat.thinking_token_budget_field.as_deref().or_else(|| {
        compat
            .supports_thinking_token_budget
            .then_some("thinking_token_budget")
    })
}

// ---------------------------------------------------------------------------
// Usage / stop mapping
// ---------------------------------------------------------------------------

/// Port of `parseChunkUsage`.
pub fn parse_chunk_usage(raw_usage: &Value, model: &Model) -> Option<Usage> {
    let prompt_tokens = raw_usage
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_read_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .or_else(|| {
            raw_usage
                .get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_i64())
        })
        .or_else(|| raw_usage.get("cached_tokens").and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = raw_usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let reasoning_tokens = raw_usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_i64());

    // OpenAI-compatible providers report cached prompt tokens inside
    // `prompt_tokens`; match upstream by exposing only uncached input here.
    let input = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens)
        .max(0);
    let output = completion_tokens;
    let cache_read = cache_read_tokens;
    let cache_write = cache_write_tokens;
    let total = input + output + cache_read + cache_write;
    let cost = calculate_cost(
        model,
        &crate::types::Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: None,
            reasoning: reasoning_tokens,
            total_tokens: total,
            cost: crate::types::Cost::default(),
        },
    );
    Some(Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: reasoning_tokens,
        total_tokens: total,
        cost,
    })
}

/// Port of `mapStopReason`.
pub fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("Provider finish_reason: content_filter".to_string()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("Provider finish_reason: network_error".to_string()),
        ),
        _ => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {reason}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

fn new_output(model: &Model) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_usage(crate::types::Usage::default());
    message.set_stop_reason(StopReason::Pending);
    message
}

fn set_error_message(message: &mut AssistantMessage, text: String) {
    let AssistantMessage::Assistant { error_message, .. } = message;
    *error_message = Some(text);
}

/// Poll the shared Rust cancellation flag used by `StreamOptions.abort_signal`.
/// JavaScript's `AbortSignal` wakes an in-flight fetch immediately; the Rust
/// option is an `Arc<AtomicBool>`, so the polling interval is the smallest
/// faithful wake-up boundary available without changing the public type.
pub(crate) async fn wait_for_abort(signal: crate::types::AbortSignal) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AbortSignalError;

/// Await a provider operation while observing `StreamOptions.abort_signal`.
/// Dropping the losing future is intentional: reqwest cancels its request/body
/// operation when the future is dropped, matching AbortSignal-driven fetch
/// cancellation for send, body, and stream reads.
pub(crate) async fn abortable<F, T>(
    future: F,
    signal: Option<crate::types::AbortSignal>,
) -> Result<T, AbortSignalError>
where
    F: Future<Output = T>,
{
    if signal
        .as_ref()
        .is_some_and(|signal| signal.load(Ordering::SeqCst))
    {
        return Err(AbortSignalError);
    }
    let Some(signal) = signal else {
        return Ok(future.await);
    };
    tokio::pin!(future);
    tokio::select! {
        value = &mut future => Ok(value),
        _ = wait_for_abort(signal) => Err(AbortSignalError),
    }
}

/// Apply the upstream-style asynchronous payload replacement hook. `None`
/// keeps the generated payload; `Some(value)` replaces it. Abort wins over a
/// hook that is still pending, just as an aborted fetch wins over request
/// construction in the JavaScript adaptors.
pub(crate) async fn apply_payload_hook(
    payload: Value,
    model: &Model,
    hook: Option<&crate::types::OnPayloadFn>,
    signal: Option<crate::types::AbortSignal>,
) -> Result<Value, AbortSignalError> {
    let Some(hook) = hook else {
        return Ok(payload);
    };
    let replacement = abortable(hook(payload.clone(), model.clone()), signal).await?;
    Ok(replacement.unwrap_or(payload))
}

pub(crate) fn signal_aborted(signal: Option<&crate::types::AbortSignal>) -> bool {
    signal.is_some_and(|signal| signal.load(Ordering::SeqCst))
}

pub(crate) fn terminal_error_message(
    model: &Model,
    text: impl Into<String>,
    aborted: bool,
) -> AssistantMessage {
    let mut message = new_output(model);
    message.set_stop_reason(if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    });
    set_error_message(&mut message, text.into());
    message
}

enum OpenAiRequestError {
    Aborted,
    Transport(String),
    RetryDelay(String),
}

struct OpenAiRequestOptions<'a> {
    client: &'a reqwest::Client,
    url: &'a str,
    params: &'a Value,
    headers: &'a ProviderHeaders,
    api_key: &'a str,
    use_bearer_auth: bool,
    timeout_ms: Option<u64>,
    max_retries: u32,
    max_retry_delay_ms: Option<u64>,
    signal: Option<crate::types::AbortSignal>,
}

/// Execute a fresh OpenAI-compatible request for each retry, matching the
/// pinned upstream `retryProviderRequest` wrapper around the SDK call.
async fn send_openai_request(
    request_options: OpenAiRequestOptions<'_>,
) -> Result<reqwest::Response, OpenAiRequestError> {
    let OpenAiRequestOptions {
        client,
        url,
        params,
        headers,
        api_key,
        use_bearer_auth,
        timeout_ms,
        max_retries,
        max_retry_delay_ms,
        signal,
    } = request_options;
    let make_request = || {
        let mut request = client.post(url).header("content-type", "application/json");
        if use_bearer_auth {
            request = request.bearer_auth(api_key);
        }
        for (name, value) in headers {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        if let Some(timeout_ms) = timeout_ms {
            request = request.timeout(Duration::from_millis(timeout_ms));
        }
        request.json(params)
    };

    let mut retry_index = 0;
    loop {
        let response = match abortable(make_request().send(), signal.clone()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if retry_index >= max_retries {
                    return Err(OpenAiRequestError::Transport(format!(
                        "Request failed: {error}"
                    )));
                }
                let delay = exponential_retry_delay(retry_index);
                if abortable(
                    tokio::time::sleep(Duration::from_millis(delay)),
                    signal.clone(),
                )
                .await
                .is_err()
                {
                    return Err(OpenAiRequestError::Aborted);
                }
                retry_index += 1;
                continue;
            }
            Err(_) => return Err(OpenAiRequestError::Aborted),
        };

        let status = response.status().as_u16();
        let should_retry = retryable_provider_status(
            status,
            response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok()),
        );
        if retry_index >= max_retries || !should_retry {
            return Ok(response);
        }

        let delay = match retry_after_delay_ms(response.headers()) {
            Some(delay) => {
                let max_delay = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
                if max_delay > 0 && delay > max_delay {
                    let provider_message = match abortable(response.bytes(), signal.clone()).await {
                        Ok(Ok(body)) => extract_openai_error(&String::from_utf8_lossy(&body)),
                        Ok(Err(error)) => format!("Request body failed: {error}"),
                        Err(_) => return Err(OpenAiRequestError::Aborted),
                    };
                    return Err(OpenAiRequestError::RetryDelay(format!(
                        "Server requested {}s retry delay (max: {}s). {}",
                        delay.div_ceil(1000),
                        max_delay.div_ceil(1000),
                        provider_message
                    )));
                }
                delay
            }
            None => exponential_retry_delay(retry_index),
        };

        drop(response);
        if abortable(
            tokio::time::sleep(Duration::from_millis(delay)),
            signal.clone(),
        )
        .await
        .is_err()
        {
            return Err(OpenAiRequestError::Aborted);
        }
        retry_index += 1;
    }
}

pub(crate) fn retryable_provider_status(status: u16, should_retry: Option<&str>) -> bool {
    match should_retry.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("true") => return true,
        Some(value) if value.eq_ignore_ascii_case("false") => return false,
        _ => {}
    }
    matches!(status, 408 | 409 | 429) || status >= 500
}

pub(crate) fn retry_after_delay_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(value) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_nonnegative_float)
    {
        return Some(value as u64);
    }
    let value = headers.get("retry-after")?.to_str().ok()?;
    parse_nonnegative_float(value)
        .map(|seconds| (seconds * 1000.0) as u64)
        .or_else(|| parse_http_date_delay_ms(value))
}

fn parse_nonnegative_float(value: &str) -> Option<f64> {
    let value = value.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_http_date_delay_ms(value: &str) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let _weekday = fields.next()?;
    let day = fields.next()?.parse::<i64>().ok()?;
    let month = match fields.next()? {
        "Jan" => 1_i64,
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
    if time.next().is_some() || fields.next()? != "GMT" || fields.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
        || year < 1
    {
        return None;
    }
    let max_day = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > max_day {
        return None;
    }

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

pub(crate) fn exponential_retry_delay(retry_index: u32) -> u64 {
    BASE_RETRY_DELAY_MS.saturating_mul(1_u64 << retry_index.min(4))
}

pub(crate) fn error_reason(aborted: bool) -> ErrorReason {
    if aborted {
        ErrorReason::Aborted
    } else {
        ErrorReason::Error
    }
}

pub(crate) fn immediate_error_stream(
    model: &Model,
    text: impl Into<String>,
    aborted: bool,
) -> AssistantMessageEventStream {
    let mut stream = AssistantMessageEventStream::new();
    let message = terminal_error_message(model, text, aborted);
    stream.push(AssistantMessageEvent::Error {
        reason: error_reason(aborted),
        error_message: message,
    });
    stream
}

/// Options for the OpenAI-completions adaptor (upstream `OpenAICompletionsOptions`).
#[derive(Clone, Default)]
pub struct OpenAIChatOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub thinking_budgets: Option<crate::types::ThinkingBudgets>,
}

impl From<&SimpleStreamOptions> for OpenAIChatOptions {
    fn from(simple: &SimpleStreamOptions) -> Self {
        Self {
            base: simple.base.clone(),
            reasoning_effort: None,
            tool_choice: simple.tool_choice,
            thinking_budgets: simple.thinking_budgets.clone(),
        }
    }
}

/// Default base URL for a completions provider (provider factory override).
pub fn default_base_url(provider: &str) -> String {
    match provider {
        "deepseek" => "https://api.deepseek.com".to_string(),
        "groq" => "https://api.groq.com/openai/v1".to_string(),
        "together" => "https://api.together.ai/v1".to_string(),
        "fireworks" => "https://api.fireworks.ai/inference".to_string(),
        "cerebras" => "https://api.cerebras.ai/v1".to_string(),
        "nvidia" => "https://integrate.api.nvidia.com/v1".to_string(),
        "moonshotai" => "https://api.moonshot.ai/v1".to_string(),
        "moonshotai-cn" => "https://api.moonshot.cn/v1".to_string(),
        "openrouter" => "https://openrouter.ai/api/v1".to_string(),
        "ant-ling" => "https://api.ant-ling.com/v1".to_string(),
        "baseten" => "https://inference.baseten.co/v1".to_string(),
        "huggingface" => "https://router.huggingface.co/v1".to_string(),
        "zai" => "https://api.z.ai/api/coding/paas/v4".to_string(),
        "zai-coding-cn" => "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
        "qwen-token-plan" => {
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1".to_string()
        }
        "qwen-token-plan-cn" => {
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1".to_string()
        }
        "qwen-token-plan-individual" => {
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1".to_string()
        }
        "xiaomi" => "https://api.xiaomimimo.com/v1".to_string(),
        "xiaomi-token-plan-ams" => "https://token-plan-ams.xiaomimimo.com/v1".to_string(),
        "xiaomi-token-plan-cn" => "https://token-plan-cn.xiaomimimo.com/v1".to_string(),
        "xiaomi-token-plan-sgp" => "https://token-plan-sgp.xiaomimimo.com/v1".to_string(),
        _ => "https://api.openai.com/v1".to_string(),
    }
}

/// Streaming adaptor for the openai-completions API family.
///
/// `options.base.api_key` carries the auth-applied key (from the Models
/// facade); `options.base.headers` are merged request headers. The events
/// mirror upstream: start → text/thinking/toolcall deltas → done|error.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &OpenAIChatOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let sender = match stream.sender() {
        Some(s) => s,
        None => return stream,
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let api_key = api_key.map(|s| s.to_string());
    let base_url = base_url.to_string();

    tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let compat = OpenAiCompletionsCompat::get(&model);
        let cache_retention = resolve_cache_retention(
            options.base.cache_retention.as_ref(),
            options.base.base.env.as_ref(),
        );
        if signal_aborted(options.base.abort_signal.as_ref()) {
            let message = terminal_error_message(&model, "Request was aborted", true);
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }
        let params = match build_params_for_chat_options(
            &model,
            &context,
            &options,
            &compat,
            &cache_retention,
        ) {
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

        // Resolve the api key: options first, else env var for common providers.
        let grammar_properties = match create_grammar_tool_input_properties(
            &context.tools,
            compat.supports_openai_grammar_tools,
        ) {
            Ok(properties) => properties,
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

        let resolved_key = match api_key {
            Some(k) => k,
            None => {
                match get_client_api_key(&model.provider, None, options.base.base.headers.as_ref())
                {
                    Ok(k) => k,
                    Err(e) => {
                        let mut message = new_output(&model);
                        message.set_stop_reason(StopReason::Error);
                        set_error_message(&mut message, e);
                        pusher.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error_message: message.clone(),
                        });
                        pusher.end(Some(message));
                        return;
                    }
                }
            }
        };

        let headers = build_request_headers(
            &model,
            &context,
            options.base.base.headers.as_ref(),
            options.base.session_id.as_deref(),
            &compat,
        );
        // An explicit Authorization header, including a null suppression
        // marker, must win over the SDK-style bearer default. This is used by
        // Cloudflare AI Gateway and by custom provider credentials.
        let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let response = match send_openai_request(OpenAiRequestOptions {
            client: &client,
            url: &endpoint,
            params: &params,
            headers: &headers,
            api_key: &resolved_key,
            use_bearer_auth: !has_header_name(&headers, "authorization"),
            timeout_ms: options.base.base.timeout_ms,
            max_retries: options.base.base.max_retries.unwrap_or(0),
            max_retry_delay_ms: options.base.base.max_retry_delay_ms,
            signal: options.base.abort_signal.clone(),
        })
        .await
        {
            Ok(response) => response,
            Err(OpenAiRequestError::Aborted) => {
                let message = terminal_error_message(&model, "Request was aborted", true);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
            Err(OpenAiRequestError::Transport(error))
            | Err(OpenAiRequestError::RetryDelay(error)) => {
                let message = terminal_error_message(&model, error, false);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
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
            let detail = extract_openai_error(&body_text);
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            set_error_message(
                &mut message,
                format!(
                    "OpenAI-completions API error ({}): {}",
                    status.as_u16(),
                    detail
                ),
            );
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }

        let body_text = String::from_utf8_lossy(&body).to_string();
        let events = crate::sse::SseParser::parse_text(&body_text);
        pusher.push(AssistantMessageEvent::Start {
            partial: new_output(&model),
        });

        match process_completions_events_with_grammar(
            &model,
            &events,
            &compat,
            &grammar_properties,
            |event| pusher.push(event),
        ) {
            Ok(mut message) => {
                if signal_aborted(options.base.abort_signal.as_ref()) {
                    message.set_stop_reason(StopReason::Aborted);
                    set_error_message(&mut message, "Request was aborted".to_string());
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Aborted,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                    return;
                }
                if message.stop_reason() == Some(StopReason::Error) {
                    let err_text = message
                        .error_message()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                    let _ = err_text;
                } else {
                    pusher.push(AssistantMessageEvent::Done {
                        reason: match message.stop_reason().unwrap_or(StopReason::Stop) {
                            StopReason::Stop | StopReason::Pending => DoneReason::Stop,
                            StopReason::Length => DoneReason::Length,
                            StopReason::ToolUse => DoneReason::ToolUse,
                            StopReason::Deferred => DoneReason::Deferred,
                            StopReason::Error | StopReason::Aborted => DoneReason::Stop,
                        },
                        message: message.clone(),
                    });
                    pusher.end(Some(message));
                }
            }
            Err(err_text) => {
                let aborted = signal_aborted(options.base.abort_signal.as_ref());
                let message = terminal_error_message(
                    &model,
                    if aborted {
                        "Request was aborted".to_string()
                    } else {
                        err_text
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
    stream
}

/// Simple streaming entrypoint (upstream `streamSimple`): clamps reasoning
/// level and delegates to `stream`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let reasoning_effort = simple_reasoning_effort(model, options.reasoning);
    let chat_options = OpenAIChatOptions {
        base: options.base.clone(),
        reasoning_effort,
        tool_choice: options.tool_choice,
        thinking_budgets: options.thinking_budgets.clone(),
    };
    stream(model, context, client, base_url, api_key, &chat_options)
}

pub(crate) fn extract_openai_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return append_openrouter_raw(msg, &value);
        }
        if let Some(msg) = value.get("message").and_then(Value::as_str) {
            return append_openrouter_raw(msg, &value);
        }
    }
    body.chars().take(300).collect()
}

fn append_openrouter_raw(message: &str, value: &Value) -> String {
    let raw = value
        .pointer("/metadata/raw")
        .or_else(|| value.pointer("/error/metadata/raw"))
        .and_then(Value::as_str);
    match raw.filter(|raw| !raw.is_empty() && !message.contains(raw)) {
        Some(raw) => format!("{message}\n{raw}"),
        None => message.to_string(),
    }
}

struct StreamingBlock {
    kind: BlockKind,
    text: String,
    thinking: String,
    thinking_signature: String,
    tool_id: String,
    tool_name: String,
    tool_arguments: Value,
    partial_args: String,
    custom_input_property: Option<String>,
    grammar_buffer: Option<GrammarToolInputJsonBuffer>,
}

enum BlockKind {
    Text,
    Thinking,
    ToolCall,
}

/// Process SSE data events using ordinary function-tool semantics.
pub fn process_completions_events(
    model: &Model,
    events: &[crate::sse::SseEvent],
    compat: &OpenAiCompletionsCompat,
    on_event: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    process_completions_events_with_grammar(model, events, compat, &BTreeMap::new(), on_event)
}

/// Process SSE data events into the unified stream protocol. Mirrors the
/// upstream for-await loop, including OpenAI custom grammar tool deltas.
pub fn process_completions_events_with_grammar(
    model: &Model,
    events: &[crate::sse::SseEvent],
    compat: &OpenAiCompletionsCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    mut on_event: impl FnMut(AssistantMessageEvent),
) -> Result<AssistantMessage, String> {
    let mut output = new_output(model);
    let mut blocks: Vec<StreamingBlock> = Vec::new();
    let mut text_block: Option<usize> = None;
    let mut thinking_block: Option<usize> = None;
    let mut tool_calls_by_index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut tool_calls_by_id: BTreeMap<String, usize> = BTreeMap::new();
    let mut has_finish_reason = false;

    let find_index = |blocks: &Vec<StreamingBlock>, target: &StreamingBlock| -> Option<usize> {
        blocks.iter().position(|b| std::ptr::eq(b, target))
    };
    let _ = find_index;

    for event in events {
        if !event.data.starts_with("data:") && event.data.trim().is_empty() {
            continue;
        }
        let data = event
            .data
            .strip_prefix("data:")
            .unwrap_or(&event.data)
            .trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        if let Some(id) = chunk.get("id").and_then(|i| i.as_str()) {
            if output.response_id().is_none() {
                output.set_response_id(id.to_string());
            }
        }
        if let Some(response_model) = chunk.get("model").and_then(|m| m.as_str()) {
            if !response_model.is_empty()
                && response_model != model.id
                && output.model() != Some(model.id.as_str())
            {
                output.set_response_model(response_model.to_string());
            }
        }
        if let Some(usage) = chunk.get("usage") {
            if let Some(parsed) = parse_chunk_usage(usage, model) {
                output.set_usage(parsed);
            }
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        else {
            continue;
        };

        // Fallback usage in choice.usage (Moonshot).
        if chunk.get("usage").is_none() {
            if let Some(usage) = choice.get("usage") {
                if let Some(parsed) = parse_chunk_usage(usage, model) {
                    output.set_usage(parsed);
                }
            }
        }

        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            output.set_raw_stop_reason(finish_reason.to_string());
            let (stop_reason, error_message) = map_stop_reason(finish_reason);
            output.set_stop_reason(stop_reason);
            if let Some(err) = error_message {
                set_error_message(&mut output, err);
            }
            has_finish_reason = true;
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                let idx = ensure_text_block(&mut blocks, &mut text_block, &mut on_event, &output);
                if let Some(block) = blocks.get_mut(idx) {
                    block.text += content;
                }
                on_event(AssistantMessageEvent::TextDelta {
                    content_index: idx,
                    delta: content.to_string(),
                    partial: output.clone(),
                });
            }
        }

        // Reasoning fields: reasoning_content, reasoning, reasoning_text.
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(value) = delta.get(field).and_then(|v| v.as_str()) {
                if !value.is_empty() {
                    let sig = if model.provider == "opencode-go" && field == "reasoning" {
                        "reasoning_content".to_string()
                    } else {
                        field.to_string()
                    };
                    let idx = ensure_thinking_block(
                        &mut blocks,
                        &mut thinking_block,
                        sig,
                        &mut on_event,
                        &output,
                    );
                    if let Some(block) = blocks.get_mut(idx) {
                        block.thinking += value;
                    }
                    on_event(AssistantMessageEvent::ThinkingDelta {
                        content_index: idx,
                        delta: value.to_string(),
                        partial: output.clone(),
                    });
                    break;
                }
            }
        }

        // OpenRouter and newer OpenAI-compatible endpoints can attach
        // structured reasoning details to a delta instead of (or alongside)
        // textual reasoning fields. Preserve the ordered detail array in the
        // thinking signature so the next request can replay it.
        if let Some(details) = delta.get("reasoning_details").and_then(|v| v.as_array()) {
            let valid_details = details
                .iter()
                .filter(|detail| is_openai_reasoning_detail(detail))
                .cloned()
                .collect::<Vec<_>>();
            if !valid_details.is_empty() {
                let idx = ensure_thinking_block(
                    &mut blocks,
                    &mut thinking_block,
                    String::new(),
                    &mut on_event,
                    &output,
                );
                if let Some(block) = blocks.get_mut(idx) {
                    let mut preserved = serde_json::from_str::<Value>(&block.thinking_signature)
                        .ok()
                        .filter(|value| value.is_array())
                        .and_then(|value| value.as_array().cloned())
                        .unwrap_or_default();
                    preserved.extend(valid_details);
                    block.thinking_signature = Value::Array(preserved).to_string();
                }
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tool_call in tool_calls {
                let index = tool_call
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .map(|i| i as usize);
                let id = tool_call
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let function = tool_call.get("function");
                let custom = tool_call.get("custom");
                let name = function
                    .and_then(|f| f.get("name"))
                    .or_else(|| custom.and_then(|f| f.get("name")))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let custom_input_property = if function.is_none() && custom.is_some() {
                    grammar_tool_input_properties
                        .get(&name)
                        .cloned()
                        .or_else(|| Some("input".to_string()))
                } else {
                    None
                };
                let idx = ensure_tool_call_block(
                    &mut blocks,
                    index,
                    &id,
                    &name,
                    custom_input_property.as_deref(),
                    &mut tool_calls_by_index,
                    &mut tool_calls_by_id,
                    &mut on_event,
                    &output,
                );
                let mut delta_str = String::new();
                if let Some(args) = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    delta_str = args.to_string();
                    if let Some(block) = blocks.get_mut(idx) {
                        block.partial_args += args;
                        block.tool_arguments = parse_streaming_json(&block.partial_args);
                    }
                } else if let Some(input) =
                    custom.and_then(|f| f.get("input")).and_then(|a| a.as_str())
                {
                    if let Some(block) = blocks.get_mut(idx) {
                        let property = block
                            .custom_input_property
                            .as_deref()
                            .unwrap_or("input")
                            .to_string();
                        let current = block.tool_arguments[&property]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let next = format!("{current}{input}");
                        let buffer = block
                            .grammar_buffer
                            .get_or_insert_with(GrammarToolInputJsonBuffer::default);
                        delta_str =
                            append_grammar_tool_input_json_delta(buffer, &property, &next, false)?
                                .unwrap_or_default();
                        block.tool_arguments = json!({ property: next });
                    }
                }
                if !delta_str.is_empty() {
                    on_event(AssistantMessageEvent::ToolCallDelta {
                        content_index: idx,
                        delta: delta_str,
                        partial: output.clone(),
                    });
                }
            }
        }
    }

    // finishBlock for every block, in order.
    let mut finished: Vec<AssistantMessageEvent> = Vec::new();
    for (idx, block) in blocks.iter_mut().enumerate() {
        let closing_delta = if let Some(property) = block.custom_input_property.clone() {
            let current = block.tool_arguments[&property]
                .as_str()
                .unwrap_or("")
                .to_string();
            let buffer = block
                .grammar_buffer
                .get_or_insert_with(GrammarToolInputJsonBuffer::default);
            let delta = append_grammar_tool_input_json_delta(buffer, &property, &current, true)?;
            block.tool_arguments = json!({ property: current });
            delta
        } else {
            None
        };
        if let Some(delta) = closing_delta {
            on_event(AssistantMessageEvent::ToolCallDelta {
                content_index: idx,
                delta,
                partial: output.clone(),
            });
        }

        match block.kind {
            BlockKind::Text => {
                finished.push(AssistantMessageEvent::TextEnd {
                    content_index: idx,
                    content: block.text.clone(),
                    partial: output.clone(),
                });
            }
            BlockKind::Thinking => {
                finished.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: idx,
                    content: block.thinking.clone(),
                    partial: output.clone(),
                });
            }
            BlockKind::ToolCall => {
                let tool_call = ContentBlock::tool_call(
                    block.tool_id.clone(),
                    block.tool_name.clone(),
                    block.tool_arguments.clone(),
                );
                finished.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call,
                    partial: output.clone(),
                });
            }
        }
    }
    for event in finished {
        on_event(event);
    }

    if !has_finish_reason && !compat.supports_finish_reason {
        let has_tool = blocks.iter().any(|b| matches!(b.kind, BlockKind::ToolCall));
        output.set_stop_reason(if has_tool {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        });
    }

    // Assemble the final content blocks into the message in block order.
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    for block in &blocks {
        match block.kind {
            BlockKind::Text => content_blocks.push(ContentBlock::text(block.text.clone())),
            BlockKind::Thinking => {
                content_blocks.push(ContentBlock::Thinking {
                    thinking: block.thinking.clone(),
                    thinking_signature: if block.thinking_signature.is_empty() {
                        None
                    } else {
                        Some(block.thinking_signature.clone())
                    },
                    redacted: None,
                });
            }
            BlockKind::ToolCall => {
                content_blocks.push(ContentBlock::tool_call(
                    block.tool_id.clone(),
                    block.tool_name.clone(),
                    block.tool_arguments.clone(),
                ));
            }
        }
    }
    output.set_content(content_blocks);

    if output.stop_reason() == Some(StopReason::Error) {
        let err = output
            .error_message()
            .map(|s| s.to_string())
            .unwrap_or_default();
        return Err(err);
    }
    if (compat.supports_finish_reason && !has_finish_reason)
        || output.stop_reason() == Some(StopReason::Pending)
    {
        return Err("Stream ended without finish_reason".to_string());
    }

    Ok(output)
}

fn is_openai_reasoning_detail(detail: &Value) -> bool {
    let Some(object) = detail.as_object() else {
        return false;
    };
    let common_fields_valid = ["id", "format", "index"].iter().all(|field| {
        object.get(*field).is_none_or(|value| match *field {
            "id" => value.is_null() || value.is_string(),
            "format" => value.is_string(),
            "index" => value.is_number(),
            _ => false,
        })
    });
    if !common_fields_valid {
        return false;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => object.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => object.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            object.get("text").is_some_and(Value::is_string)
                && object
                    .get("signature")
                    .is_none_or(|value| value.is_null() || value.is_string())
        }
        _ => false,
    }
}

fn parse_openai_reasoning_details(signature: &str) -> Option<Vec<Value>> {
    let details = serde_json::from_str::<Value>(signature).ok()?;
    let details = details.as_array()?;
    if details.is_empty() || !details.iter().all(is_openai_reasoning_detail) {
        return None;
    }
    Some(details.to_vec())
}

fn ensure_text_block(
    blocks: &mut Vec<StreamingBlock>,
    text_block: &mut Option<usize>,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    if let Some(existing) = *text_block {
        return existing;
    }
    {
        blocks.push(StreamingBlock {
            kind: BlockKind::Text,
            text: String::new(),
            thinking: String::new(),
            thinking_signature: String::new(),
            tool_id: String::new(),
            tool_name: String::new(),
            tool_arguments: Value::Null,
            partial_args: String::new(),
            custom_input_property: None,
            grammar_buffer: None,
        });
        let idx = blocks.len() - 1;
        *text_block = Some(idx);
        on_event(AssistantMessageEvent::TextStart {
            content_index: idx,
            partial: output.clone(),
        });
        idx
    }
}

fn ensure_thinking_block(
    blocks: &mut Vec<StreamingBlock>,
    thinking_block: &mut Option<usize>,
    signature: String,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    if let Some(existing) = *thinking_block {
        return existing;
    }
    {
        blocks.push(StreamingBlock {
            kind: BlockKind::Thinking,
            text: String::new(),
            thinking: String::new(),
            thinking_signature: signature,
            tool_id: String::new(),
            tool_name: String::new(),
            tool_arguments: Value::Null,
            partial_args: String::new(),
            custom_input_property: None,
            grammar_buffer: None,
        });
        let idx = blocks.len() - 1;
        *thinking_block = Some(idx);
        on_event(AssistantMessageEvent::ThinkingStart {
            content_index: idx,
            partial: output.clone(),
        });
        idx
    }
}

#[allow(clippy::too_many_arguments)]
fn ensure_tool_call_block(
    blocks: &mut Vec<StreamingBlock>,
    stream_index: Option<usize>,
    id: &str,
    name: &str,
    custom_input_property: Option<&str>,
    by_index: &mut BTreeMap<usize, usize>,
    by_id: &mut BTreeMap<String, usize>,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    let existing = stream_index
        .and_then(|i| by_index.get(&i).copied())
        .or_else(|| {
            if id.is_empty() {
                None
            } else {
                by_id.get(id).copied()
            }
        });
    if let Some(idx) = existing {
        if let Some(block) = blocks.get_mut(idx) {
            if block.tool_id.is_empty() && !id.is_empty() {
                block.tool_id = id.to_string();
                by_id.insert(id.to_string(), idx);
            }
            if block.tool_name.is_empty() && !name.is_empty() {
                block.tool_name = name.to_string();
            }
            if let Some(property) = custom_input_property {
                if block.custom_input_property.is_none() {
                    block.custom_input_property = Some(property.to_string());
                    block.tool_arguments = json!({ property: "" });
                    block.grammar_buffer = Some(GrammarToolInputJsonBuffer::default());
                    block.partial_args.clear();
                }
            }
        }
        return idx;
    }
    let is_custom = custom_input_property.is_some();
    blocks.push(StreamingBlock {
        kind: BlockKind::ToolCall,
        text: String::new(),
        thinking: String::new(),
        thinking_signature: String::new(),
        tool_id: id.to_string(),
        tool_name: name.to_string(),
        tool_arguments: custom_input_property
            .map(|property| json!({ property: "" }))
            .unwrap_or_else(|| serde_json::Map::new().into()),
        partial_args: String::new(),
        custom_input_property: custom_input_property.map(str::to_string),
        grammar_buffer: is_custom.then(GrammarToolInputJsonBuffer::default),
    });
    let idx = blocks.len() - 1;
    if let Some(i) = stream_index {
        by_index.insert(i, idx);
    }
    if !id.is_empty() {
        by_id.insert(id.to_string(), idx);
    }
    on_event(AssistantMessageEvent::ToolCallStart {
        content_index: idx,
        partial: output.clone(),
    });
    idx
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::AssistantMessage;
    use crate::types::{ContentBlock, Message, Tool, UserContent};

    fn model(id: &str, provider: &str) -> Model {
        let mut m = Model::new(id, id, "openai-completions", provider);
        m.base_url = format!("https://{provider}.example.com/v1");
        m
    }

    fn context(system: Option<&str>, messages: Vec<Message>, tools: Vec<Tool>) -> Context {
        Context {
            system_prompt: system.map(|s| s.to_string()),
            messages,
            tools,
        }
    }

    #[test]
    fn detect_compat_openai_defaults() {
        let m = model("gpt-5", "openai");
        let compat = OpenAiCompletionsCompat::get(&m);
        assert!(compat.supports_store);
        assert!(compat.supports_developer_role);
        assert!(compat.supports_reasoning_effort);
        assert_eq!(compat.max_tokens_field, "max_completion_tokens");
        assert_eq!(compat.thinking_format, "openai");
        assert!(compat.supports_strict_mode);
        assert_eq!(compat.session_affinity_format, "openai");
    }

    #[test]
    fn detect_compat_deepseek() {
        let mut m = model("deepseek-chat", "deepseek");
        m.base_url = "https://api.deepseek.com".to_string();
        let compat = OpenAiCompletionsCompat::get(&m);
        assert_eq!(compat.max_tokens_field, "max_tokens");
        assert_eq!(compat.thinking_format, "deepseek");
        assert!(compat.requires_reasoning_content_on_assistant_messages);
        assert!(!compat.supports_store);
        assert!(!compat.supports_developer_role);
    }

    #[test]
    fn detect_compat_openrouter() {
        let m = model("anthropic/claude-sonnet", "openrouter");
        let compat = OpenAiCompletionsCompat::get(&m);
        assert_eq!(compat.session_affinity_format, "openrouter");
        assert_eq!(compat.cache_control_format.as_deref(), Some("anthropic"));
        assert!(compat.supports_developer_role);
    }

    #[test]
    fn openrouter_session_affinity_header_is_opt_in_and_overridable() {
        let m = model("anthropic/claude-sonnet", "openrouter");
        let context = context(None, vec![], vec![]);
        let mut compat = OpenAiCompletionsCompat::get(&m);

        let headers = build_request_headers(&m, &context, None, Some("session-1"), &compat);
        assert!(!headers.contains_key("x-session-id"));

        compat.send_session_affinity_headers = true;
        let headers = build_request_headers(&m, &context, None, Some("session-1"), &compat);
        assert_eq!(
            headers.get("x-session-id"),
            Some(&Some("session-1".to_string()))
        );
        assert!(!headers.contains_key("session_id"));
        assert!(!headers.contains_key("x-client-request-id"));

        let mut overrides = ProviderHeaders::new();
        overrides.insert("x-session-id".to_string(), Some("override".to_string()));
        let headers =
            build_request_headers(&m, &context, Some(&overrides), Some("session-1"), &compat);
        assert_eq!(
            headers.get("x-session-id"),
            Some(&Some("override".to_string()))
        );
    }

    #[test]
    fn detect_compat_matches_target_openai_compatible_providers() {
        for provider in ["groq", "huggingface"] {
            let compat = OpenAiCompletionsCompat::get(&model("target-model", provider));
            assert!(compat.supports_store, "{provider} should support store");
            assert!(
                compat.supports_developer_role,
                "{provider} should support developer messages"
            );
            assert!(
                compat.supports_reasoning_effort,
                "{provider} should support reasoning effort"
            );
            assert_eq!(compat.max_tokens_field, "max_completion_tokens");
            assert!(compat.supports_strict_mode);
            assert_eq!(compat.thinking_format, "openai");
        }

        let compat = OpenAiCompletionsCompat::get(&model("moonshot-v1-8k", "moonshotai"));
        assert!(!compat.supports_store);
        assert!(!compat.supports_developer_role);
        assert!(!compat.supports_reasoning_effort);
        assert_eq!(compat.max_tokens_field, "max_tokens");
        assert!(!compat.supports_strict_mode);
        assert_eq!(compat.thinking_format, "openai");
    }

    #[test]
    fn xiaomi_mimo_replays_tool_calls_and_deepseek_thinking_wire_shape() {
        let model = crate::providers::catalog_models("xiaomi")
            .into_iter()
            .find(|model| model.id == "mimo-v2.5-pro")
            .expect("pinned Xiaomi MiMo model");
        let compat = OpenAiCompletionsCompat::get(&model);
        assert_eq!(compat.thinking_format, "deepseek");
        assert!(compat.requires_reasoning_content_on_assistant_messages);
        assert!(compat.supports_store);
        assert_eq!(compat.max_tokens_field, "max_completion_tokens");

        let mut assistant = AssistantMessage::new();
        assistant.set_content(vec![ContentBlock::tool_call(
            "call_1",
            "read",
            json!({"path":"README.md"}),
        )]);
        let context = context(
            None,
            vec![
                Message::User(UserContent::string("Read README.md", 1)),
                Message::Assistant(assistant),
                Message::ToolResult(crate::types::ToolResultMessage::text(
                    "call_1", "read", "contents", false,
                )),
            ],
            vec![],
        );
        let options = StreamOptions {
            sampling_params: Some(json!({"reasoningEffort":"high"})),
            max_tokens: Some(4096),
            ..Default::default()
        };

        let params = build_params(&model, &context, Some(&options), &compat, "none")
            .expect("Xiaomi request parameters");
        assert_eq!(params["thinking"], json!({"type":"enabled"}));
        assert_eq!(params["reasoning_effort"], json!("high"));
        assert_eq!(params["max_completion_tokens"], json!(4096));
        assert_eq!(params["stream_options"]["include_usage"], json!(true));
        assert_eq!(params["store"], json!(false));

        let replayed = params["messages"]
            .as_array()
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|message| message["role"] == "assistant")
            })
            .expect("replayed assistant tool call");
        assert_eq!(replayed["reasoning_content"], json!(""));
        assert_eq!(replayed["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(replayed["tool_calls"][0]["function"]["name"], json!("read"));
        assert_eq!(
            replayed["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"README.md"}"#)
        );

        let off_params = build_params(&model, &context, None, &compat, "none")
            .expect("Xiaomi no-thinking parameters");
        assert_eq!(off_params["thinking"], json!({"type":"disabled"}));
        assert!(off_params.get("reasoning_effort").is_none());
    }

    #[test]
    fn target_catalog_overrides_match_groq_huggingface_and_moonshot_wire_contracts() {
        let groq = crate::providers::catalog_models("groq")
            .into_iter()
            .find(|model| model.id == "qwen/qwen3.6-27b")
            .expect("Groq Qwen reasoning model");
        let groq_compat = OpenAiCompletionsCompat::get(&groq);
        assert!(groq.reasoning);
        assert!(groq_compat.supports_store);
        assert!(groq_compat.supports_developer_role);
        assert!(groq_compat.supports_reasoning_effort);
        assert_eq!(groq_compat.max_tokens_field, "max_completion_tokens");

        let huggingface = crate::providers::catalog_models("huggingface")
            .into_iter()
            .find(|model| model.id == "MiniMaxAI/MiniMax-M2")
            .expect("Hugging Face MiniMax model");
        let huggingface_compat = OpenAiCompletionsCompat::get(&huggingface);
        assert!(huggingface.reasoning);
        assert!(!huggingface_compat.supports_developer_role);
        assert!(huggingface_compat.supports_reasoning_effort);
        assert_eq!(huggingface_compat.max_tokens_field, "max_completion_tokens");

        let moonshot = crate::providers::catalog_models("moonshotai")
            .into_iter()
            .find(|model| model.id == "kimi-k3")
            .expect("Moonshot Kimi K3 model");
        let moonshot_compat = OpenAiCompletionsCompat::get(&moonshot);
        assert!(moonshot.reasoning);
        assert!(moonshot_compat.supports_reasoning_effort);
        assert!(moonshot_compat.requires_reasoning_content_on_assistant_messages);
        assert_eq!(moonshot_compat.max_tokens_field, "max_tokens");
        assert_eq!(moonshot_compat.deferred_tools_mode.as_deref(), Some("kimi"));
    }

    #[test]
    fn together_reasoning_uses_upstream_enabled_wire_field() {
        let model = crate::providers::catalog_models("together")
            .into_iter()
            .find(|model| model.id == "MiniMaxAI/MiniMax-M3")
            .expect("Together reasoning model");
        let compat = OpenAiCompletionsCompat::get(&model);
        assert_eq!(compat.thinking_format, "together");
        assert!(!compat.supports_reasoning_effort);

        let options = StreamOptions {
            sampling_params: Some(json!({ "reasoningEffort": "medium" })),
            ..Default::default()
        };
        let enabled = build_params(&model, &Context::default(), Some(&options), &compat, "none")
            .expect("Together enabled parameters");
        assert_eq!(enabled["reasoning"], json!({ "enabled": true }));
        assert!(enabled.get("reasoning_effort").is_none());

        let disabled = build_params(&model, &Context::default(), None, &compat, "none")
            .expect("Together disabled parameters");
        assert_eq!(disabled["reasoning"], json!({ "enabled": false }));
    }

    #[test]
    fn kimi_deferred_tools_are_replayed_after_tool_results_and_removed_from_active_tools() {
        let model = crate::providers::catalog_models("moonshotai")
            .into_iter()
            .find(|model| model.id == "kimi-k3")
            .expect("Moonshot Kimi K3 model");
        let compat = OpenAiCompletionsCompat::get(&model);
        let active_tool = crate::types::json_tool(
            "active",
            "available immediately",
            &json!({"type":"object","properties":{}}),
        );
        let deferred_tool = crate::types::json_tool(
            "deferred",
            "loaded by the tool result",
            &json!({"type":"object","properties":{}}),
        );
        let mut assistant = AssistantMessage::new();
        assistant.set_content(vec![ContentBlock::tool_call(
            "load-1",
            "loader",
            json!({"query":"deferred"}),
        )]);
        let mut result = crate::types::ToolResultMessage::text("load-1", "loader", "loaded", false);
        let crate::types::ToolResultMessage::ToolResult {
            added_tool_names, ..
        } = &mut result;
        *added_tool_names = Some(vec!["deferred".to_string()]);
        let context = Context {
            messages: vec![
                Message::User(UserContent::string("load the tool", 1)),
                Message::Assistant(assistant),
                Message::ToolResult(result),
                Message::User(UserContent::string("continue", 2)),
            ],
            tools: vec![active_tool, deferred_tool],
            ..Default::default()
        };

        let params = build_params(&model, &context, None, &compat, "none").unwrap();
        let active_tools = params["tools"].as_array().unwrap();
        assert_eq!(active_tools.len(), 1);
        assert_eq!(active_tools[0]["function"]["name"], "active");

        let messages = params["messages"].as_array().unwrap();
        let roles: Vec<_> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "system", "user"]);
        assert!(messages[3].get("content").is_none());
        assert_eq!(messages[3]["tools"][0]["function"]["name"], "deferred");
    }

    #[test]
    fn simple_options_forward_tool_choice_to_openai_compatible_payload() {
        let model = model("gpt-5", "groq");
        let options = OpenAIChatOptions {
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        };
        let params = build_params_for_chat_options(
            &model,
            &Context::default(),
            &options,
            &OpenAiCompletionsCompat::get(&model),
            "none",
        )
        .unwrap();
        assert_eq!(params["tool_choice"], json!("none"));
    }

    #[test]
    fn openai_compatible_provider_catalogs_preserve_pinned_metadata() {
        let baseten = crate::providers::catalog_models("baseten")
            .into_iter()
            .find(|model| model.id == "zai-org/GLM-5.2")
            .expect("Baseten GLM 5.2 model");
        assert_eq!(baseten.name, "GLM 5.2");
        assert_eq!(
            baseten.input,
            vec![
                crate::model::ModelInput::Text,
                crate::model::ModelInput::Image
            ]
        );
        assert_eq!(baseten.context_window, 1_048_576);
        assert_eq!(baseten.max_tokens, 262_144);
        assert_eq!(
            baseten
                .thinking_level_map
                .as_ref()
                .and_then(|levels| levels.get(&crate::types::ModelThinkingLevel::High)),
            Some(&Some("high".to_string()))
        );
        assert_eq!(
            baseten
                .compat
                .as_ref()
                .and_then(|compat| compat.get("supportsReasoningEffort"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let cerebras = crate::providers::catalog_models("cerebras")
            .into_iter()
            .find(|model| model.id == "gpt-oss-120b")
            .expect("Cerebras GPT OSS 120B model");
        assert_eq!(cerebras.base_url, "https://api.cerebras.ai/v1");
        assert_eq!(cerebras.max_tokens, 40_960);
        assert_eq!(
            cerebras
                .thinking_level_map
                .as_ref()
                .and_then(|levels| levels.get(&crate::types::ModelThinkingLevel::Low)),
            Some(&Some("low".to_string()))
        );
    }

    #[test]
    fn retry_policy_honors_server_override_and_retry_after_headers() {
        assert!(retryable_provider_status(400, Some("true")));
        assert!(!retryable_provider_status(503, Some("false")));
        assert!(retryable_provider_status(429, None));
        assert!(!retryable_provider_status(400, None));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after-ms",
            reqwest::header::HeaderValue::from_static("250"),
        );
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("2"),
        );
        assert_eq!(retry_after_delay_ms(&headers), Some(250));

        headers.remove("retry-after-ms");
        assert_eq!(retry_after_delay_ms(&headers), Some(2_000));
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("0.5"),
        );
        assert_eq!(retry_after_delay_ms(&headers), Some(500));
        assert_eq!(exponential_retry_delay(0), 500);
        assert_eq!(exponential_retry_delay(4), 8_000);
        assert_eq!(exponential_retry_delay(12), 8_000);
    }

    #[test]
    fn qwen_token_plan_uses_enable_thinking_and_maps_effort() {
        let model = crate::providers::catalog_models("qwen-token-plan")
            .into_iter()
            .find(|model| model.id == "qwen3.8-max")
            .expect("Qwen Token Plan international model");
        let compat = OpenAiCompletionsCompat::get(&model);
        assert_eq!(compat.thinking_format, "qwen");
        assert!(compat.supports_reasoning_effort);

        let options = StreamOptions {
            sampling_params: Some(json!({"reasoningEffort":"xhigh"})),
            ..Default::default()
        };
        let params = build_params(&model, &Context::default(), Some(&options), &compat, "none")
            .expect("Qwen Token Plan request params");
        assert_eq!(params["enable_thinking"], json!(true));
        assert_eq!(params["reasoning_effort"], json!("xhigh"));
        assert!(params.get("thinking").is_none());

        let off = StreamOptions::default();
        let off_params = build_params(&model, &Context::default(), Some(&off), &compat, "none")
            .expect("Qwen Token Plan off params");
        assert_eq!(off_params["enable_thinking"], json!(false));
        assert!(off_params.get("reasoning_effort").is_none());
        assert!(off_params.get("thinking").is_none());
    }

    #[test]
    fn moonshot_deepseek_omits_thinking_when_off_is_null() {
        let model = crate::providers::catalog_models("moonshotai")
            .into_iter()
            .find(|model| model.id == "kimi-k2.7-code")
            .expect("Moonshot Kimi K2.7 Code model");
        let compat = OpenAiCompletionsCompat::get(&model);
        assert_eq!(compat.thinking_format, "deepseek");
        assert_eq!(
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::types::ModelThinkingLevel::Off)),
            Some(&None)
        );

        let params = build_params(&model, &Context::default(), None, &compat, "none")
            .expect("Moonshot request params");
        assert!(params.get("thinking").is_none());
    }

    #[test]
    fn qwen_token_plan_stream_simple_preserves_extended_effort_after_clamping() {
        for provider_id in [
            "qwen-token-plan",
            "qwen-token-plan-cn",
            "qwen-token-plan-individual",
        ] {
            let model = crate::providers::catalog_models(provider_id)
                .into_iter()
                .find(|model| model.id == "qwen3.8-max")
                .expect("Qwen Token Plan qwen3.8-max model");
            let compat = OpenAiCompletionsCompat::get(&model);
            let effort = simple_reasoning_effort(&model, Some(crate::types::ThinkingLevel::Xhigh));
            assert_eq!(effort.as_deref(), Some("xhigh"), "{provider_id}");
            let max_effort =
                simple_reasoning_effort(&model, Some(crate::types::ThinkingLevel::Max));
            assert_eq!(max_effort.as_deref(), Some("xhigh"), "{provider_id}");

            let params = build_params_for_chat_options(
                &model,
                &Context::default(),
                &OpenAIChatOptions {
                    reasoning_effort: effort,
                    ..Default::default()
                },
                &compat,
                "none",
            )
            .expect("Qwen Token Plan request params");
            assert_eq!(params["enable_thinking"], json!(true), "{provider_id}");
            assert_eq!(params["reasoning_effort"], json!("xhigh"), "{provider_id}");
            assert!(params.get("thinking").is_none(), "{provider_id}");
        }
    }

    #[test]
    fn local_openai_compatible_thinking_budget_uses_explicit_field_and_alias() {
        let mut model = model("local-model", "llama.cpp");
        model.reasoning = true;
        model.max_tokens = 16_384;
        model.compat = Some(json!({
            "thinkingFormat": "zai",
            "supportsThinkingTokenBudget": true
        }));
        let options = OpenAIChatOptions {
            reasoning_effort: Some("medium".to_string()),
            thinking_budgets: Some(crate::types::ThinkingBudgets {
                medium: Some(4_096),
                ..Default::default()
            }),
            ..Default::default()
        };
        let compat = OpenAiCompletionsCompat::get(&model);
        let params =
            build_params_for_chat_options(&model, &Context::default(), &options, &compat, "none")
                .expect("local llama.cpp-compatible request params");
        assert_eq!(params["thinking_token_budget"], json!(4_096));

        model.compat = Some(json!({
            "thinkingFormat": "qwen",
            "supportsThinkingTokenBudget": true,
            "thinkingTokenBudgetField": "thinking_budget_tokens"
        }));
        let compat = OpenAiCompletionsCompat::get(&model);
        let params =
            build_params_for_chat_options(&model, &Context::default(), &options, &compat, "none")
                .expect("explicit local budget field request params");
        assert_eq!(params["thinking_budget_tokens"], json!(4_096));
        assert!(params.get("thinking_token_budget").is_none());

        let off = OpenAIChatOptions {
            thinking_budgets: Some(crate::types::ThinkingBudgets {
                high: Some(8_192),
                ..Default::default()
            }),
            ..Default::default()
        };
        let params =
            build_params_for_chat_options(&model, &Context::default(), &off, &compat, "none")
                .expect("local off request params");
        assert!(params.get("thinking_budget_tokens").is_none());
    }

    #[test]
    fn map_stop_reason_cases() {
        assert_eq!(map_stop_reason("stop").0, StopReason::Stop);
        assert_eq!(map_stop_reason("end").0, StopReason::Stop);
        assert_eq!(map_stop_reason("length").0, StopReason::Length);
        assert_eq!(map_stop_reason("tool_calls").0, StopReason::ToolUse);
        assert_eq!(map_stop_reason("function_call").0, StopReason::ToolUse);
        assert_eq!(map_stop_reason("content_filter").0, StopReason::Error);
        assert!(map_stop_reason("weird").1.is_some());
    }

    #[test]
    fn parse_chunk_usage_maps_fields() {
        let m = model("gpt-5", "openai");
        let raw = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 3, "cache_write_tokens": 2 },
            "completion_tokens_details": { "reasoning_tokens": 1 }
        });
        let usage = parse_chunk_usage(&raw, &m).unwrap();
        assert_eq!(usage.input, 5);
        assert_eq!(usage.output, 5);
        assert_eq!(usage.cache_read, 3);
        assert_eq!(usage.cache_write, 2);
        assert_eq!(usage.reasoning, Some(1));
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn openrouter_error_preserves_raw_metadata_without_duplicate() {
        assert_eq!(
            extract_openai_error(
                r#"{"error":{"message":"Provider returned error","metadata":{"raw":"WAF policy"}}}"#
            ),
            "Provider returned error\nWAF policy"
        );
        assert_eq!(
            extract_openai_error(
                r#"{"message":"Provider returned error","metadata":{"raw":"WAF policy"}}"#
            ),
            "Provider returned error\nWAF policy"
        );
        assert_eq!(
            extract_openai_error(
                r#"{"error":{"message":"WAF policy","metadata":{"raw":"WAF policy"}}}"#
            ),
            "WAF policy"
        );
    }

    #[test]
    fn convert_messages_text_and_tool_roundtrip() {
        let m = model("gpt-5", "openai");
        let ctx = context(
            Some("You are helpful"),
            vec![
                Message::User(UserContent::string("hello", 1)),
                Message::Assistant(assistant_with_text("hi there")),
                Message::ToolResult(crate::types::ToolResultMessage::text(
                    "call-1", "bash", "ok", false,
                )),
            ],
            vec![crate::types::json_tool(
                "bash",
                "Run a command",
                &json!({"type":"object","properties":{}}),
            )],
        );
        let compat = OpenAiCompletionsCompat::get(&m);
        let messages = convert_messages(&m, &ctx, &compat).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "hi there");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["content"], "ok");
        assert_eq!(messages[3]["tool_call_id"], "call-1");
    }

    #[test]
    fn convert_messages_developer_role_for_reasoning_model() {
        let mut m = model("gpt-5", "openai");
        m.reasoning = true;
        let ctx = context(Some("sys"), vec![], vec![]);
        let compat = OpenAiCompletionsCompat::get(&m);
        let messages = convert_messages(&m, &ctx, &compat).unwrap();
        assert_eq!(messages[0]["role"], "developer");
    }

    #[test]
    fn convert_tools_function_shape() {
        let tools = vec![crate::types::json_tool(
            "bash",
            "Run",
            &json!({"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}),
        )];
        let compat = OpenAiCompletionsCompat::get(&model("gpt-5", "openai"));
        let converted = convert_tools(&tools, &compat).unwrap();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["function"]["name"], "bash");
        assert_eq!(converted[0]["function"]["parameters"]["type"], "object");
        assert_eq!(converted[0]["function"]["strict"], false);
    }

    #[test]
    fn build_params_openai_shape() {
        let m = model("gpt-5", "openai");
        let ctx = context(
            Some("sys"),
            vec![Message::User(UserContent::string("hi", 1))],
            vec![],
        );
        let compat = OpenAiCompletionsCompat::get(&m);
        let params = build_params(&m, &ctx, None, &compat, "short").unwrap();
        assert_eq!(params["model"], "gpt-5");
        assert_eq!(params["stream"], true);
        assert_eq!(params["stream_options"]["include_usage"], true);
        assert_eq!(params["store"], false);
        assert!(params.get("tools").is_none());
    }

    #[test]
    fn optional_prompt_cache_key_is_omitted_without_session_id() {
        let mut m = model("gpt-5", "openai");
        m.base_url = "https://api.openai.com/v1".to_string();
        let compat = OpenAiCompletionsCompat::get(&m);
        let opts = StreamOptions {
            cache_retention: Some("short".to_string()),
            ..Default::default()
        };
        let params = build_params(
            &m,
            &context(None, vec![], vec![]),
            Some(&opts),
            &compat,
            "short",
        )
        .unwrap();
        assert!(params.get("prompt_cache_key").is_none());
        assert!(params.get("prompt_cache_retention").is_none());

        let opts = StreamOptions {
            cache_retention: Some("short".to_string()),
            session_id: Some("session-123".to_string()),
            ..Default::default()
        };
        let params = build_params(
            &m,
            &context(None, vec![], vec![]),
            Some(&opts),
            &compat,
            "short",
        )
        .unwrap();
        assert_eq!(params["prompt_cache_key"], json!("session-123"));
    }

    #[test]
    fn openrouter_anthropic_cache_control_marks_instruction_tools_and_last_message() {
        let model = model("anthropic/claude-sonnet", "openrouter");
        let context = context(
            Some("System prompt"),
            vec![Message::User(UserContent::string("Hello", 1))],
            vec![crate::types::json_tool(
                "read",
                "Read a file",
                &json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                }),
            )],
        );
        let compat = OpenAiCompletionsCompat::get(&model);
        let params = build_params(&model, &context, None, &compat, "short").unwrap();
        let marker = json!({"type": "ephemeral"});

        assert_eq!(params["messages"][0]["content"][0]["cache_control"], marker);
        assert_eq!(params["tools"][0]["cache_control"], marker);
        assert_eq!(params["messages"][1]["content"][0]["cache_control"], marker);
    }

    #[test]
    fn openrouter_anthropic_cache_control_is_omitted_for_none_retention() {
        let model = model("anthropic/claude-sonnet", "openrouter");
        let context = context(
            Some("System prompt"),
            vec![Message::User(UserContent::string("Hello", 1))],
            vec![crate::types::json_tool(
                "read",
                "Read a file",
                &json!({"type": "object", "properties": {}}),
            )],
        );
        let compat = OpenAiCompletionsCompat::get(&model);
        let params = build_params(&model, &context, None, &compat, "none").unwrap();

        assert!(params["messages"][0]["content"].is_string());
        assert!(params["messages"][1]["content"].is_string());
        assert!(params["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn process_events_text_stream() {
        let sse = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-1","usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}

data: [DONE]
"#;
        let m = model("gpt-5", "openai");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let mut received = Vec::new();
        let result = process_completions_events(&m, &events, &compat, |e| received.push(e));
        let msg = result.unwrap();
        assert_eq!(msg.stop_reason(), Some(StopReason::Stop));
        let text: String = msg
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(received
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextStart { .. })));
        assert!(received.iter().any(
            |e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == " world")
        ));
        // Done is pushed by the stream() wrapper, not the pure processor.
        assert!(!received
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Done { .. })));
    }

    #[test]
    fn process_events_tool_call_stream() {
        let sse = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]
"#;
        let m = model("gpt-5", "openai");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let result = process_completions_events(&m, &events, &compat, |_| {});
        let msg = result.unwrap();
        assert_eq!(msg.stop_reason(), Some(StopReason::ToolUse));
        let tool_calls: Vec<&ContentBlock> = msg
            .content()
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
            .collect();
        assert_eq!(tool_calls.len(), 1);
        match tool_calls[0] {
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "bash");
                assert_eq!(arguments["cmd"], "ls");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn converts_grammar_tools_to_openai_custom_shape() {
        let mut tool = crate::types::json_tool(
            "sample",
            "sample text",
            &json!({
                "type": "object",
                "properties": {"payload": {"type": "string"}},
                "required": ["payload"]
            }),
        );
        let mut variants = BTreeMap::new();
        variants.insert("openai_lark".to_string(), "start: /[a-z]+/".to_string());
        tool.constrained_sampling = Some(crate::types::ConstrainedSampling::Grammar { variants });
        let mut compat = OpenAiCompletionsCompat::get(&model("gpt-5", "openai"));
        compat.supports_openai_grammar_tools = true;
        let converted = convert_tools(&[tool], &compat).unwrap();
        assert_eq!(converted[0]["type"], "custom");
        assert_eq!(converted[0]["custom"]["format"]["type"], "grammar");
        assert_eq!(
            converted[0]["custom"]["format"]["grammar"]["syntax"],
            "lark"
        );
    }

    #[test]
    fn process_events_grammar_custom_tool_stream() {
        let sse = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"custom","custom":{"name":"sample","input":"ab"}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]
"#;
        let m = model("gpt-5", "openai");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let mut properties = BTreeMap::new();
        properties.insert("sample".to_string(), "payload".to_string());
        let result =
            process_completions_events_with_grammar(&m, &events, &compat, &properties, |_| {})
                .unwrap();
        assert_eq!(result.stop_reason(), Some(StopReason::ToolUse));
        assert_eq!(
            result.content()[0],
            ContentBlock::tool_call("call_1", "sample", json!({"payload":"ab"}))
        );
    }

    #[test]
    fn required_unsupported_schema_returns_upstream_diagnostic() {
        let mut tool = crate::types::json_tool(
            "sample",
            "sample text",
            &json!({"type":"object","properties":{},"allOf":[]}),
        );
        tool.constrained_sampling = Some(crate::types::ConstrainedSampling::JsonSchema {
            strict: crate::types::StrictPreference::Require,
        });
        let compat = OpenAiCompletionsCompat::get(&model("gpt-5", "openai"));
        assert_eq!(
            convert_tools(&[tool], &compat).unwrap_err(),
            "Tool \"sample\" requires JSON-schema constrained sampling, but allOf schemas are unsupported."
        );
    }

    #[test]
    fn process_events_thinking_stream() {
        let sse = r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"Let me think"}}]}

data: {"choices":[{"index":0,"delta":{"content":"Answer"}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
"#;
        let m = model("deepseek-reasoner", "deepseek");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let result = process_completions_events(&m, &events, &compat, |_| {});
        let msg = result.unwrap();
        let thinking: Vec<&ContentBlock> = msg
            .content()
            .iter()
            .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
            .collect();
        assert_eq!(thinking.len(), 1);
        assert!(
            matches!(thinking[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think")
        );
    }

    #[test]
    fn process_events_preserves_structured_reasoning_details() {
        let sse = r#"data: {"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.encrypted","id":"r1","data":"opaque"}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"content":"Answer"},"finish_reason":"stop"}]}

data: [DONE]
"#;
        let m = model("openrouter-model", "openrouter");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let result = process_completions_events(&m, &events, &compat, |_| {}).unwrap();
        let signature = match &result.content()[0] {
            ContentBlock::Thinking {
                thinking_signature: Some(signature),
                ..
            } => signature,
            other => panic!("expected preserved reasoning signature, got {other:?}"),
        };
        let details: Value = serde_json::from_str(signature).unwrap();
        assert_eq!(details[0]["type"], "reasoning.encrypted");
        assert_eq!(details[0]["data"], "opaque");

        let replayed = convert_messages(
            &m,
            &context(None, vec![Message::Assistant(result)], vec![]),
            &compat,
        )
        .unwrap();
        assert_eq!(
            replayed[0]["reasoning_details"][0]["type"],
            "reasoning.encrypted"
        );
        assert_eq!(replayed[0]["reasoning_details"][0]["data"], "opaque");
    }

    #[test]
    fn process_events_missing_finish_reason_errors() {
        let sse = r#"data: {"choices":[{"index":0,"delta":{"content":"oops"}}]}
"#;
        let m = model("gpt-5", "openai");
        let compat = OpenAiCompletionsCompat::get(&m);
        let events = crate::sse::SseParser::parse_text(sse);
        let result = process_completions_events(&m, &events, &compat, |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn normalize_tool_call_id_pipe_separated() {
        let long = format!("call_A|{}", "x".repeat(300));
        let n = normalize_tool_call_id(&long, "github-copilot");
        assert!(n.len() <= 40);
        assert!(!n.contains('|'));
        // openai: truncate at 40
        let n2 = normalize_tool_call_id(&"a".repeat(50), "openai");
        assert_eq!(n2.len(), 40);
    }

    #[test]
    fn transform_messages_drops_empty_same_model_thinking_signature() {
        let model = model("gpt-5", "openai");
        let mut assistant = AssistantMessage::new();
        assistant.set_api_provider_model("openai-completions", "openai", "gpt-5");
        assistant.set_content(vec![ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: Some(String::new()),
            redacted: None,
        }]);

        let transformed = transform_messages(&model, &[Message::Assistant(assistant)]);

        assert!(matches!(
            &transformed[0],
            Message::Assistant(message) if message.content().is_empty()
        ));
    }

    fn assistant_with_text(text: &str) -> AssistantMessage {
        let mut a = AssistantMessage::new();
        a.set_content(vec![ContentBlock::text(text)]);
        a
    }
}
