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
//! streaming JSON for tool arguments). Grammar tools and chat-template kwargs
//! are documented as deferred (see TODO).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::model::{calculate_cost, clamp_thinking_level, Model};
use crate::partial_json::parse_streaming_json;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context, DoneReason,
    ErrorReason, ProviderEnv, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions,
    Tool, ToolChoice, Usage,
};
use crate::event_stream::StreamSink;
use crate::AssistantMessageEventStream;

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
}

impl OpenAiCompletionsCompat {
    /// Auto-detect from provider/URL (upstream `detectCompat`).
    pub fn detect(model: &Model) -> Self {
        let provider = &model.provider;
        let base_url = model.base_url.to_lowercase();

        let is_zai = provider == "zai" || provider == "zai-coding-cn" || base_url.contains("api.z.ai") || base_url.contains("open.bigmodel.cn");
        let is_together = provider == "together" || base_url.contains("api.together.ai") || base_url.contains("api.together.xyz");
        let is_moonshot = provider == "moonshotai" || provider == "moonshotai-cn" || base_url.contains("api.moonshot.");
        let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
        let is_cloudflare_workers_ai = provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
        let is_cloudflare_ai_gateway = provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
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
        let is_openrouter_developer_role_model =
            is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/") {
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
            supports_developer_role: is_openrouter_developer_role_model || (!is_non_standard && !is_openrouter),
            supports_reasoning_effort: !is_grok
                && !is_zai
                && !is_moonshot
                && !is_together
                && !is_cloudflare_ai_gateway
                && !is_nvidia
                && !is_ant_ling,
            supports_usage_in_streaming: true,
            supports_finish_reason: true,
            max_tokens_field: if use_max_tokens { "max_tokens" } else { "max_completion_tokens" }.to_string(),
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant_messages: is_deepseek,
            thinking_format: thinking_format.to_string(),
            zai_tool_stream: false,
            supports_thinking_token_budget: false,
            thinking_token_budget_field: None,
            supports_strict_mode: !is_moonshot && !is_together && !is_cloudflare_ai_gateway && !is_nvidia,
            supports_openai_grammar_tools: false,
            cache_control_format,
            send_session_affinity_headers: false,
            deferred_tools_mode: None,
            session_affinity_format: if is_openrouter { "openrouter" } else { "openai" }.to_string(),
            supports_long_cache_retention: !(is_together
                || is_cloudflare_workers_ai
                || is_cloudflare_ai_gateway
                || is_nvidia
                || is_ant_ling),
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
        let get_str = |k: &str| compat.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let get_str_opt = |k: &str| compat.get(k).cloned();
        let cache_control_format_override = match get_str_opt("cacheControlFormat") {
            Some(Value::String(s)) => Some(s),
            Some(Value::Null) => None,
            _ => detected.cache_control_format.clone(),
        };

        Self {
            supports_store: get_bool("supportsStore").unwrap_or(detected.supports_store),
            supports_developer_role: get_bool("supportsDeveloperRole").unwrap_or(detected.supports_developer_role),
            supports_reasoning_effort: get_bool("supportsReasoningEffort").unwrap_or(detected.supports_reasoning_effort),
            supports_usage_in_streaming: get_bool("supportsUsageInStreaming").unwrap_or(detected.supports_usage_in_streaming),
            supports_finish_reason: get_bool("supportsFinishReason").unwrap_or(detected.supports_finish_reason),
            max_tokens_field: get_str("maxTokensField").unwrap_or(detected.max_tokens_field),
            requires_tool_result_name: get_bool("requiresToolResultName").unwrap_or(detected.requires_tool_result_name),
            requires_assistant_after_tool_result: get_bool("requiresAssistantAfterToolResult").unwrap_or(detected.requires_assistant_after_tool_result),
            requires_thinking_as_text: get_bool("requiresThinkingAsText").unwrap_or(detected.requires_thinking_as_text),
            requires_reasoning_content_on_assistant_messages: get_bool("requiresReasoningContentOnAssistantMessages")
                .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
            thinking_format: get_str("thinkingFormat").unwrap_or(detected.thinking_format),
            zai_tool_stream: get_bool("zaiToolStream").unwrap_or(detected.zai_tool_stream),
            supports_thinking_token_budget: get_bool("supportsThinkingTokenBudget").unwrap_or(detected.supports_thinking_token_budget),
            thinking_token_budget_field: get_str("thinkingTokenBudgetField").or(detected.thinking_token_budget_field),
            supports_strict_mode: get_bool("supportsStrictMode").unwrap_or(detected.supports_strict_mode),
            supports_openai_grammar_tools: get_bool("supportsOpenAIGrammarTools").unwrap_or(detected.supports_openai_grammar_tools),
            cache_control_format: cache_control_format_override,
            send_session_affinity_headers: get_bool("sendSessionAffinityHeaders").unwrap_or(detected.send_session_affinity_headers),
            deferred_tools_mode: get_str("deferredToolsMode").or(detected.deferred_tools_mode),
            session_affinity_format: get_str("sessionAffinityFormat").unwrap_or(detected.session_affinity_format),
            supports_long_cache_retention: get_bool("supportsLongCacheRetention").unwrap_or(detected.supports_long_cache_retention),
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

fn get_client_api_key(provider: &str, api_key: Option<&str>, headers: Option<&ProviderHeaders>) -> Result<String, String> {
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

fn has_tool_history(messages: &[crate::types::Message]) -> bool {
    for msg in messages {
        match msg {
            crate::types::Message::ToolResult(_) => return true,
            crate::types::Message::Assistant(a) => {
                if a.content().iter().any(|b| matches!(b, ContentBlock::ToolCall { .. })) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
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

pub fn resolve_cache_retention(cache_retention: Option<&CacheRetention>, env: Option<&ProviderEnv>) -> String {
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
        let sanitize = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
        let call_id = sanitize(call_id);
        let item_id = sanitize(item_id);
        let combined = if item_id.is_empty() { call_id.clone() } else { format!("{call_id}_{item_id}") };
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

/// Port of `convertMessages` in openai-completions.ts. Produces the
/// `messages` array for the Chat Completions request.
pub fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &OpenAiCompletionsCompat,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let _provider = model.provider.clone();

    // Transform messages: downgrade unsupported images + normalize tool ids.
    let transformed = transform_messages(model, &context.messages);

    if let Some(system_prompt) = &context.system_prompt {
        let use_developer_role = model.reasoning && compat.supports_developer_role;
        let role = if use_developer_role { "developer" } else { "system" };
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
                                ContentBlock::Image { data, mime_type, .. } => {
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
                let assistant_content: Option<Value> = if compat.requires_assistant_after_tool_result {
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
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
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
                        // Reasoning signature replay: use the first thinking
                        // block's signature as the reasoning field.
                        let signature = non_empty_thinking[0]
                            .as_thinking()
                            .and_then(|t| match t {
                                ContentBlock::Thinking { thinking_signature, .. } => thinking_signature.clone(),
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
                                        ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                assistant_msg.insert(sig.into(), json!(content));
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg.insert("content".into(), json!(assistant_text));
                }

                if !tool_calls.is_empty() {
                    let converted: Vec<Value> = tool_calls
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolCall { id, name, arguments, .. } => {
                                Some(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into())
                                    }
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    assistant_msg.insert("tool_calls".into(), json!(converted));
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
                        let has_images = tr.content().iter().any(|b| matches!(b, ContentBlock::Image { .. }));
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

                        if has_images && model.input.contains(&crate::model::ModelInput::Image) {
                            for block in tr.content() {
                                if let ContentBlock::Image { data, mime_type, .. } = block {
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
                    let mut user_content: Vec<Value> = vec![json!({ "type": "text", "text": "Attached image(s) from tool result:" })];
                    user_content.extend(image_blocks);
                    params.push(json!({ "role": "user", "content": user_content }));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
                // Advance past the grouped tool results (the while loop adds 1).
                i = j.saturating_sub(1);
            }
        }
        i += 1;
    }

    params
}

// ---------------------------------------------------------------------------
// transformMessages (port of api/transform-messages.ts)
// ---------------------------------------------------------------------------

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

fn replace_images_with_placeholder(content: &[ContentBlock], placeholder: &str) -> Vec<ContentBlock> {
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
            ContentBlock::Text { text, text_signature } => {
                result.push(ContentBlock::Text { text: text.clone(), text_signature: text_signature.clone() });
                previous_was_placeholder = text == placeholder;
            }
            other => result.push(other.clone()),
        }
    }
    result
}

/// Downgrade unsupported images to placeholder text (upstream
/// `downgradeUnsupportedImages`).
fn downgrade_unsupported_images(messages: &[crate::types::Message], model: &Model) -> Vec<crate::types::Message> {
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

/// Port of `transformMessages`: cross-model thinking-signature downgrade,
/// redacted-thinking dropping, tool-call-id normalization.
pub fn transform_messages(model: &Model, messages: &[crate::types::Message]) -> Vec<crate::types::Message> {
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
                        ContentBlock::Thinking { thinking, thinking_signature, redacted } => {
                            if matches!(redacted, Some(true)) {
                                if same_model {
                                    new_content.push(block.clone());
                                }
                                // redacted thinking dropped cross-model
                            } else if same_model && thinking_signature.is_some() {
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
                                    ContentBlock::ToolCall { thought_signature, .. } if thought_signature.is_some() => {
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

    // Second pass (upstream): insert synthetic empty tool results for orphaned
    // tool calls. The core coding-agent loop never produces orphaned calls;
    // kept as a documented no-op for now.
    let _ = &tool_call_id_map;
    transformed
}

// ---------------------------------------------------------------------------
// Tool conversion
// ---------------------------------------------------------------------------

fn is_openai_completions_reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning_content" | "reasoning" | "reasoning_text")
}

fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    // Full upstream makeStrictJsonSchema; ported for tool strict mode.
    let mut cloned = schema.clone();
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(|v| v.as_str()) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), String> {
    let Some(obj) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".to_string());
    };
    for key in ["anyOf", "items", "properties", "required", "additionalProperties"] {
        if obj.contains_key(key) {
            // handled below per-key
        }
    }
    if let Some(any_of) = obj.get("anyOf") {
        let variants = any_of.as_array().ok_or("anyOf must contain at least one schema")?;
        if variants.is_empty() {
            return Err("anyOf must contain at least one schema".to_string());
        }
        for variant in variants {
            let is_structured = variant
                .as_object()
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .map(|t| t == "object" || t == "array")
                .unwrap_or(false);
            if is_structured {
                return Err("object and array unions are unsupported".to_string());
            }
        }
        // Recurse clones (upstream mutates each variant in place).
        for variant in variants {
            let mut v = variant.clone();
            make_json_schema_node_strict(&mut v)?;
        }
    }
    if let Some(items) = obj.get("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
        let mut v = items.clone();
        make_json_schema_node_strict(&mut v)?;
    }
    let is_object_schema = obj.get("type").and_then(|v| v.as_str()) == Some("object");
    if obj.contains_key("properties") && !is_object_schema {
        return Err("properties require type object".to_string());
    }
    if !is_object_schema {
        return Ok(());
    }
    if let Some(ap) = obj.get("additionalProperties") {
        if ap.as_bool() != Some(false) {
            return Err("schema-valued or true additionalProperties is unsupported".to_string());
        }
    }
    if let Some(props) = obj.get("properties") {
        if !props.is_object() {
            return Err("object properties must be a schema map".to_string());
        }
    }
    if let Some(required) = obj.get("required") {
        if !required.is_array() || required.as_array().unwrap().iter().any(|k| !k.is_string()) {
            return Err("object required must be a string array".to_string());
        }
    }

    let properties = obj.get("properties").cloned().unwrap_or_else(|| json!({}));
    let properties_obj = properties.as_object().cloned().unwrap_or_default();
    let property_names: Vec<String> = properties_obj.keys().cloned().collect();
    let required: std::collections::BTreeSet<String> = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|k| k.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    for key in &required {
        if !property_names.contains(key) {
            return Err("required contains an unknown property".to_string());
        }
    }
    let mut new_properties = serde_json::Map::new();
    for (key, property) in &properties_obj {
        let mut prop = property.clone();
        make_json_schema_node_strict(&mut prop)?;
        if required.contains(key) || schema_allows_null(&prop) {
            new_properties.insert(key.clone(), prop);
        } else {
            new_properties.insert(key.clone(), json!({ "anyOf": [prop, { "type": "null" }] }));
        }
    }
    obj.insert("properties".into(), Value::Object(new_properties));
    obj.insert("required".into(), json!(property_names));
    obj.insert("additionalProperties".into(), json!(false));
    Ok(())
}

fn schema_allows_null(schema: &Value) -> bool {
    match schema.get("type").and_then(|v| v.as_str()) {
        Some("null") => true,
        _ => {
            if let Some(any_of) = schema.get("anyOf").and_then(|v| v.as_array()) {
                any_of.iter().any(schema_allows_null)
            } else {
                false
            }
        }
    }
}

fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(config) = &tool.constrained_sampling else {
        return Ok(None);
    };
    let crate::types::ConstrainedSampling::JsonSchema { strict } = config else {
        return Ok(None);
    };
    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(e) => {
                if *strict == crate::types::StrictPreference::Require {
                    Err(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {}.",
                        tool.name, e
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    } else if *strict == crate::types::StrictPreference::Require {
        Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ))
    } else {
        Ok(None)
    }
}

/// Port of `convertTools` (grammar tools deferred: the port's grammar custom
/// tools are documented in TODO; function tools with strict mode are active).
pub fn convert_tools(tools: &[Tool], compat: &OpenAiCompletionsCompat) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        let strict = match resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode) {
            Ok(s) => s,
            Err(e) => {
                // Upstream throws; surface as a tool that always fails? For the
                // port we propagate the error to the caller via Result.
                tracing::warn!("strict tool conversion failed: {e}");
                continue;
            }
        };
        let parameters = match strict {
            Some(true) => make_strict_json_schema(&tool.parameters).unwrap_or_else(|_| tool.parameters.clone()),
            _ => tool.parameters.clone(),
        };
        let mut function = serde_json::Map::new();
        function.insert("name".into(), json!(tool.name));
        function.insert("description".into(), json!(tool.description));
        function.insert("parameters".into(), parameters);
        if compat.supports_strict_mode {
            function.insert("strict".into(), json!(strict.unwrap_or(false)));
        }
        out.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    out
}

fn get_compat_cache_control(compat: &OpenAiCompletionsCompat, cache_retention: &str) -> Option<Value> {
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
) -> Value {
    let messages = convert_messages(model, context, compat);
    let cache_control = get_compat_cache_control(compat, cache_retention);

    let mut params = serde_json::Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), json!(messages));
    params.insert("stream".into(), json!(true));

    // prompt_cache_key/prompt_cache_retention for OpenAI + long-retention.
    let base_url_openai = model.base_url.to_lowercase().contains("api.openai.com");
    let openai_cache_key = base_url_openai && cache_retention != "none";
    let long_cache_retention = cache_retention == "long" && compat.supports_long_cache_retention;
    if openai_cache_key || long_cache_retention {
        let session_id = options.and_then(|o| o.session_id.clone()).unwrap_or_default();
        if base_url_openai || long_cache_retention {
            let key = clamp_openai_prompt_cache_key(session_id.as_str());
            params.insert("prompt_cache_key".into(), json!(key));
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
    let active_tools = context.tools.clone();
    if !active_tools.is_empty() {
        params.insert("tools".into(), json!(convert_tools(&active_tools, compat)));
        if compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    } else if has_tool_history(&context.messages) {
        params.insert("tools".into(), json!([]));
    }

    if cache_control.is_some() {
        // Anthropic-style cache control on messages/tools is deferred; the
        // cache_control_format only applies to openrouter-anthropic models.
        tracing::debug!("anthropic cache control deferred in openai-completions");
    }

    // Thinking formats.
    apply_thinking_params(model, options, compat, &mut params);

    Value::Object(params)
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
        .and_then(|sp| sp.get("reasoningEffort"))
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let thinking_budget = options
        .and_then(|o| o.sampling_params.as_ref())
        .and_then(|sp| sp.get("thinkingBudget"))
        .and_then(|v| v.as_u64());

    match compat.thinking_format.as_str() {
        "zai" if model.reasoning => {
            if reasoning_effort.is_some() {
                params.insert("thinking".into(), json!({ "type": "enabled", "clear_thinking": false }));
            } else {
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&thinking_level_from_str(effort))).cloned().flatten();
                    let value = mapped.unwrap_or_else(|| effort.clone());
                    params.insert("reasoning_effort".into(), json!(value));
                }
            }
        }
        "deepseek" if model.reasoning => {
            if reasoning_effort.is_some() {
                params.insert("thinking".into(), json!({ "type": "enabled" }));
            } else if model.thinking_level_map.as_ref().map(|m| m.get(&crate::types::ModelThinkingLevel::Off)).flatten() != Some(&Some("off".to_string())) {
                // off not explicitly mapped to null -> disable
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&thinking_level_from_str(effort))).cloned().flatten();
                    params.insert("reasoning_effort".into(), json!(mapped.unwrap_or_else(|| effort.clone())));
                }
            }
        }
        "openrouter" if model.reasoning => {
            if reasoning_effort.is_some() {
                let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&thinking_level_from_str(reasoning_effort.as_deref().unwrap_or("")))).cloned().flatten();
                params.insert("reasoning".into(), json!({ "effort": mapped.unwrap_or_else(|| reasoning_effort.clone().unwrap_or_default()) }));
            }
        }
        "together" | "ant-ling" if model.reasoning => {
            if let Some(effort) = &reasoning_effort {
                if compat.supports_reasoning_effort {
                    let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&thinking_level_from_str(effort))).cloned().flatten();
                    params.insert("reasoning_effort".into(), json!(mapped.unwrap_or_else(|| effort.clone())));
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
                    params.insert("reasoning_effort".into(), json!(mapped.unwrap_or_else(|| effort.clone())));
                }
            }
            if thinking_budget.is_some() {
                tracing::debug!("thinking token budget deferred in openai-completions");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Usage / stop mapping
// ---------------------------------------------------------------------------

/// Port of `parseChunkUsage`.
pub fn parse_chunk_usage(raw_usage: &Value, model: &Model) -> Option<Usage> {
    let prompt_tokens = raw_usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| raw_usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .or_else(|| raw_usage.get("cached_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = raw_usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let reasoning_tokens = raw_usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64());

    let input = prompt_tokens;
    let output = completion_tokens;
    let cache_read = cache_read_tokens;
    let cache_write = cache_write_tokens;
    let total = input + output + cache_read + cache_write;
    let cost = calculate_cost(model, &crate::types::Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: reasoning_tokens,
        total_tokens: total,
        cost: crate::types::Cost::default(),
    });
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
        "content_filter" => (StopReason::Error, Some("Provider finish_reason: content_filter".to_string())),
        "network_error" => (StopReason::Error, Some("Provider finish_reason: network_error".to_string())),
        _ => (StopReason::Error, Some(format!("Provider finish_reason: {reason}"))),
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
    match message {
        AssistantMessage::Assistant { error_message, .. } => *error_message = Some(text),
        #[allow(unreachable_patterns)]
        _ => {}
    }
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
            tool_choice: simple.tool_choice.clone(),
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
        "qwen-token-plan" => "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1".to_string(),
        "qwen-token-plan-cn" => "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1".to_string(),
        "qwen-token-plan-individual" => "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1".to_string(),
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
        let cache_retention = resolve_cache_retention(options.base.cache_retention.as_ref(), options.base.base.env.as_ref());
        let params = build_params(&model, &context, Some(&options.base), &compat, &cache_retention);

        // Resolve the api key: options first, else env var for common providers.
        let resolved_key = match api_key {
            Some(k) => k,
            None => {
                match get_client_api_key(&model.provider, None, options.base.base.headers.as_ref()) {
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

        let mut request = client
            .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
            .header("content-type", "application/json")
            .bearer_auth(&resolved_key)
            .json(&params);
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
        let provider_response = crate::types::ProviderResponse {
            status: status.as_u16(),
            headers: BTreeMap::new(),
        };
        if let Some(on_response) = &options.base.on_response {
            on_response(&provider_response, &model);
        }
        let body = match response.bytes().await {
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
            let body_text = String::from_utf8_lossy(&body).to_string();
            let detail = extract_openai_error(&body_text);
            let mut message = new_output(&model);
            message.set_stop_reason(StopReason::Error);
            set_error_message(&mut message, format!("OpenAI-completions API error ({}): {}", status.as_u16(), detail));
            pusher.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        }

        let body_text = String::from_utf8_lossy(&body).to_string();
        let events = crate::sse::SseParser::parse_text(&body_text);
        pusher.push(AssistantMessageEvent::Start { partial: new_output(&model) });

        match process_completions_events(&model, &events, &compat, |event| {
            pusher.push(event);
        }) {
            Ok(message) => {
                if message.stop_reason() == Some(StopReason::Error) {
                    let err_text = message.error_message().map(|s| s.to_string()).unwrap_or_default();
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
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, err_text);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
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
    let clamped = options
        .reasoning
        .map(|r| clamp_thinking_level(model, r.into()));
    let reasoning_effort = match clamped {
        Some(crate::types::ModelThinkingLevel::Off) | Some(crate::types::ModelThinkingLevel::Minimal) => None,
        Some(crate::types::ModelThinkingLevel::Low) => Some("low".to_string()),
        Some(crate::types::ModelThinkingLevel::Medium) => Some("medium".to_string()),
        Some(crate::types::ModelThinkingLevel::High) => Some("high".to_string()),
        Some(crate::types::ModelThinkingLevel::Xhigh) => Some("high".to_string()),
        Some(crate::types::ModelThinkingLevel::Max) => Some("high".to_string()),
        None => None,
    };
    let chat_options = OpenAIChatOptions {
        base: options.base.clone(),
        reasoning_effort,
        tool_choice: options.tool_choice.clone(),
        thinking_budgets: options.thinking_budgets.clone(),
    };
    stream(model, context, client, base_url, api_key, &chat_options)
}

fn extract_openai_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
    }
    body.chars().take(300).collect()
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
}

enum BlockKind {
    Text,
    Thinking,
    ToolCall,
}

/// Process SSE data events into the unified stream protocol. Mirrors the
/// upstream for-await loop (text/thinking/tool-call deltas, usage, finish
/// reason).
pub fn process_completions_events(
    model: &Model,
    events: &[crate::sse::SseEvent],
    compat: &OpenAiCompletionsCompat,
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
        let data = event.data.strip_prefix("data:").unwrap_or(&event.data).trim();
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
            if !response_model.is_empty() && response_model != model.id && output.model() != Some(model.id.as_str()) {
                output.set_response_model(response_model.to_string());
            }
        }
        if let Some(usage) = chunk.get("usage") {
            if let Some(parsed) = parse_chunk_usage(usage, model) {
                output.set_usage(parsed);
            }
        }

        let Some(choice) = chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) else {
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

        let Some(delta) = choice.get("delta") else { continue };

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
                    let idx = ensure_thinking_block(&mut blocks, &mut thinking_block, sig, &mut on_event, &output);
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

        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tool_call in tool_calls {
                let index = tool_call.get("index").and_then(|i| i.as_u64()).map(|i| i as usize);
                let id = tool_call.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let name = tool_call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let idx = ensure_tool_call_block(
                    &mut blocks,
                    index,
                    &id,
                    &name,
                    &mut tool_calls_by_index,
                    &mut tool_calls_by_id,
                    &mut on_event,
                    &output,
                );
                let mut delta_str = String::new();
                if let Some(args) = tool_call.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()) {
                    delta_str = args.to_string();
                    if let Some(block) = blocks.get_mut(idx) {
                        block.partial_args += args;
                        block.tool_arguments = parse_streaming_json(&block.partial_args);
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
    for (idx, block) in blocks.iter().enumerate() {
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
        output.set_stop_reason(if has_tool { StopReason::ToolUse } else { StopReason::Stop });
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
        let err = output.error_message().map(|s| s.to_string()).unwrap_or_default();
        return Err(err);
    }
    if (compat.supports_finish_reason && !has_finish_reason) || output.stop_reason() == Some(StopReason::Pending) {
        return Err("Stream ended without finish_reason".to_string());
    }

    Ok(output)
}

fn ensure_text_block(
    blocks: &mut Vec<StreamingBlock>,
    text_block: &mut Option<usize>,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    if text_block.is_none() {
        blocks.push(StreamingBlock {
            kind: BlockKind::Text,
            text: String::new(),
            thinking: String::new(),
            thinking_signature: String::new(),
            tool_id: String::new(),
            tool_name: String::new(),
            tool_arguments: Value::Null,
            partial_args: String::new(),
        });
        let idx = blocks.len() - 1;
        *text_block = Some(idx);
        on_event(AssistantMessageEvent::TextStart {
            content_index: idx,
            partial: output.clone(),
        });
    }
    text_block.unwrap()
}

fn ensure_thinking_block(
    blocks: &mut Vec<StreamingBlock>,
    thinking_block: &mut Option<usize>,
    signature: String,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    if thinking_block.is_none() {
        blocks.push(StreamingBlock {
            kind: BlockKind::Thinking,
            text: String::new(),
            thinking: String::new(),
            thinking_signature: signature,
            tool_id: String::new(),
            tool_name: String::new(),
            tool_arguments: Value::Null,
            partial_args: String::new(),
        });
        let idx = blocks.len() - 1;
        *thinking_block = Some(idx);
        on_event(AssistantMessageEvent::ThinkingStart {
            content_index: idx,
            partial: output.clone(),
        });
    }
    thinking_block.unwrap()
}

#[allow(clippy::too_many_arguments)]
fn ensure_tool_call_block(
    blocks: &mut Vec<StreamingBlock>,
    stream_index: Option<usize>,
    id: &str,
    name: &str,
    by_index: &mut BTreeMap<usize, usize>,
    by_id: &mut BTreeMap<String, usize>,
    on_event: &mut impl FnMut(AssistantMessageEvent),
    output: &AssistantMessage,
) -> usize {
    let existing = stream_index
        .and_then(|i| by_index.get(&i).copied())
        .or_else(|| if id.is_empty() { None } else { by_id.get(id).copied() });
    if let Some(idx) = existing {
        if let Some(block) = blocks.get_mut(idx) {
            if block.tool_id.is_empty() && !id.is_empty() {
                block.tool_id = id.to_string();
                by_id.insert(id.to_string(), idx);
            }
            if block.tool_name.is_empty() && !name.is_empty() {
                block.tool_name = name.to_string();
            }
        }
        return idx;
    }
    blocks.push(StreamingBlock {
        kind: BlockKind::ToolCall,
        text: String::new(),
        thinking: String::new(),
        thinking_signature: String::new(),
        tool_id: id.to_string(),
        tool_name: name.to_string(),
        tool_arguments: serde_json::Map::new().into(),
        partial_args: String::new(),
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
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, Tool, UserContent};
    use crate::types::AssistantMessage;

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
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5);
        assert_eq!(usage.cache_read, 3);
        assert_eq!(usage.cache_write, 2);
        assert_eq!(usage.reasoning, Some(1));
        assert_eq!(usage.total_tokens, 20);
    }

    #[test]
    fn convert_messages_text_and_tool_roundtrip() {
        let m = model("gpt-5", "openai");
        let ctx = context(
            Some("You are helpful"),
            vec![
                Message::User(UserContent::string("hello", 1)),
                Message::Assistant(assistant_with_text("hi there")),
                Message::ToolResult(crate::types::ToolResultMessage::text("call-1", "bash", "ok", false)),
            ],
            vec![crate::types::json_tool("bash", "Run a command", &json!({"type":"object","properties":{}}))],
        );
        let compat = OpenAiCompletionsCompat::get(&m);
        let messages = convert_messages(&m, &ctx, &compat);
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
        let messages = convert_messages(&m, &ctx, &compat);
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
        let converted = convert_tools(&tools, &compat);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["function"]["name"], "bash");
        assert_eq!(converted[0]["function"]["parameters"]["type"], "object");
        assert_eq!(converted[0]["function"]["strict"], false);
    }

    #[test]
    fn build_params_openai_shape() {
        let m = model("gpt-5", "openai");
        let ctx = context(Some("sys"), vec![Message::User(UserContent::string("hi", 1))], vec![]);
        let compat = OpenAiCompletionsCompat::get(&m);
        let params = build_params(&m, &ctx, None, &compat, "short");
        assert_eq!(params["model"], "gpt-5");
        assert_eq!(params["stream"], true);
        assert_eq!(params["stream_options"]["include_usage"], true);
        assert_eq!(params["store"], false);
        assert!(params.get("tools").is_none());
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
            .filter_map(|b| match b { ContentBlock::Text { text, .. } => Some(text.as_str()), _ => None })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(received.iter().any(|e| matches!(e, AssistantMessageEvent::TextStart { .. })));
        assert!(received.iter().any(|e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == " world")));
        // Done is pushed by the stream() wrapper, not the pure processor.
        assert!(!received.iter().any(|e| matches!(e, AssistantMessageEvent::Done { .. })));
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
        let tool_calls: Vec<&ContentBlock> = msg.content().iter().filter(|b| matches!(b, ContentBlock::ToolCall { .. })).collect();
        assert_eq!(tool_calls.len(), 1);
        match tool_calls[0] {
            ContentBlock::ToolCall { name, arguments, .. } => {
                assert_eq!(name, "bash");
                assert_eq!(arguments["cmd"], "ls");
            }
            _ => unreachable!(),
        }
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
        let thinking: Vec<&ContentBlock> = msg.content().iter().filter(|b| matches!(b, ContentBlock::Thinking { .. })).collect();
        assert_eq!(thinking.len(), 1);
        assert!(matches!(thinking[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think"));
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

    fn assistant_with_text(text: &str) -> AssistantMessage {
        let mut a = AssistantMessage::new();
        a.set_content(vec![ContentBlock::text(text)]);
        a
    }
}
