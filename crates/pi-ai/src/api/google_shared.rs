//! Shared utilities for the Google Generative AI and Vertex adaptors — port
//! of `packages/ai/src/api/google-shared.ts`.

use serde_json::{json, Value};

use crate::model::Model;
use crate::types::{
    ContentBlock, Context, Message, ModelThinkingLevel, StopReason, UserContent,
    UserContentBody,
};

use super::transform_messages::transform_messages;

/// Google API thinking level values (mirrors Google's ThinkingLevel enum).
pub type GoogleApiThinkingLevel = &'static str;

/// Thinking level after resolving a pi level or model-specific mapping to a
/// standard level (xhigh/max excluded — Google has no such levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGoogleThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedGoogleThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolvedGoogleThinkingLevel::Minimal => "minimal",
            ResolvedGoogleThinkingLevel::Low => "low",
            ResolvedGoogleThinkingLevel::Medium => "medium",
            ResolvedGoogleThinkingLevel::High => "high",
        }
    }
}

/// Resolve a supported pi level or model-specific Google mapping to a
/// standard Google level (upstream `resolveGoogleThinkingLevel`).
pub fn resolve_google_thinking_level(level: ModelThinkingLevel, model: &Model) -> ResolvedGoogleThinkingLevel {
    if level == ModelThinkingLevel::Off {
        return ResolvedGoogleThinkingLevel::High;
    }
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&level))
        .cloned()
        .flatten();
    let resolved = match mapped {
        Some(s) => s.to_lowercase(),
        None => model_level_string(&level).to_string(),
    };
    match resolved.as_str() {
        "minimal" => ResolvedGoogleThinkingLevel::Minimal,
        "low" => ResolvedGoogleThinkingLevel::Low,
        "medium" => ResolvedGoogleThinkingLevel::Medium,
        "high" => ResolvedGoogleThinkingLevel::High,
        other => panic!(
            "Unsupported Google thinking level mapping for {}/{}: {:?} -> {}",
            model.provider, model.id, level, other
        ),
    }
}

fn model_level_string(level: &ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

/// A streamed Gemini part is thinking when `thought == true`.
pub fn is_thinking_part(part: &Value) -> bool {
    part.get("thought").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Retain the last non-empty thought signature within a streamed block
/// (some backends only send it on the first delta).
pub fn retain_thought_signature(
    existing: Option<&str>,
    incoming: Option<&str>,
) -> Option<String> {
    match incoming {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => existing.map(|s| s.to_string()),
    }
}

const BASE64_SIGNATURE_PATTERN: &str = "^[A-Za-z0-9+/]+={0,2}$";

fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    match signature {
        Some(s) => {
            s.len() % 4 == 0
                && regex::Regex::new(BASE64_SIGNATURE_PATTERN)
                    .map(|re| re.is_match(s))
                    .unwrap_or(false)
        }
        None => false,
    }
}

fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    if is_same_provider_and_model && is_valid_thought_signature(signature) {
        signature.map(|s| s.to_string())
    } else {
        None
    }
}

fn get_gemini_major_version(model_id: &str) -> Option<u32> {
    let lower = model_id.to_lowercase();
    let rest = lower.strip_prefix("gemini")?;
    let rest = rest.strip_prefix("-live").unwrap_or(rest);
    let rest = rest.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Models via Google APIs that require explicit tool call IDs.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    let gemini_major_version = get_gemini_major_version(model_id);
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version.is_some_and(|v| v >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    match get_gemini_major_version(model_id) {
        Some(v) => v >= 3,
        None => true,
    }
}

/// Convert internal messages to Gemini `contents` (a JSON array). Returns
/// `Value` objects shaped like Gemini `Content[]`.
pub fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let normalize_tool_call_id = |id: &str, _model: &Model, _source: &crate::types::AssistantMessage| {
        if !requires_tool_call_id(&model.id) {
            return id.to_string();
        }
        let normalized: String = id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        normalized.chars().take(64).collect()
    };

    let transformed = transform_messages(&context.messages, model, Some(&normalize_tool_call_id));

    for msg in transformed {
        match msg {
            Message::User(UserContent::RoleUser { content, .. }) => match content {
                UserContentBody::String(s) => {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{ "text": s }],
                    }));
                }
                UserContentBody::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            ContentBlock::Text { text, .. } => json!({ "text": text }),
                            ContentBlock::Image { data, mime_type, .. } => json!({
                                "inlineData": { "mimeType": mime_type, "data": data },
                            }),
                            _ => json!({}),
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut parts: Vec<Value> = Vec::new();
                let is_same_provider_and_model = assistant.provider() == Some(&model.provider)
                    && assistant.model() == Some(&model.id);

                for block in assistant.content() {
                    match block {
                        ContentBlock::Text { text, text_signature } => {
                            let thought_signature =
                                resolve_thought_signature(is_same_provider_and_model, text_signature.as_deref());
                            if text.trim().is_empty() && thought_signature.is_none() {
                                continue;
                            }
                            let mut part = json!({ "text": text });
                            if let Some(sig) = thought_signature {
                                part["thoughtSignature"] = json!(sig);
                            }
                            parts.push(part);
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            ..
                        } => {
                            if is_same_provider_and_model {
                                let thought_signature = resolve_thought_signature(
                                    is_same_provider_and_model,
                                    thinking_signature.as_deref(),
                                );
                                if thinking.trim().is_empty() && thought_signature.is_none() {
                                    continue;
                                }
                                let mut part = json!({ "thought": true, "text": thinking });
                                if let Some(sig) = thought_signature {
                                    part["thoughtSignature"] = json!(sig);
                                }
                                parts.push(part);
                            } else {
                                if thinking.trim().is_empty() {
                                    continue;
                                }
                                parts.push(json!({ "text": thinking }));
                            }
                        }
                        ContentBlock::ToolCall { id, name, arguments, thought_signature, .. } => {
                            let sig = resolve_thought_signature(is_same_provider_and_model, thought_signature.as_deref());
                            let mut function_call = json!({
                                "name": name,
                                "args": arguments,
                            });
                            if requires_tool_call_id(&model.id) {
                                function_call["id"] = json!(id);
                            }
                            let mut part = json!({ "functionCall": function_call });
                            if let Some(s) = sig {
                                part["thoughtSignature"] = json!(s);
                            }
                            parts.push(part);
                        }
                        _ => {}
                    }
                }

                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({
                    "role": "model",
                    "parts": parts,
                }));
            }
            Message::ToolResult(result) => {
                let text_content: Vec<String> = result
                    .content()
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                let text_result = text_content.join("\n");
                let image_content: Vec<&ContentBlock> = if model.input.contains(&crate::model::ModelInput::Image)
                {
                    result
                        .content()
                        .iter()
                        .filter(|c| matches!(c, ContentBlock::Image { .. }))
                        .collect()
                } else {
                    Vec::new()
                };

                let has_text = !text_result.is_empty();
                let has_images = !image_content.is_empty();
                let multimodal_ok = supports_multimodal_function_response(&model.id);

                let response_value = if has_text {
                    text_result
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };

                let image_parts: Vec<Value> = image_content
                    .iter()
                    .map(|image_block| match image_block {
                        ContentBlock::Image { data, mime_type, .. } => json!({
                            "inlineData": { "mimeType": mime_type, "data": data },
                        }),
                        _ => json!({}),
                    })
                    .collect();

                let include_id = requires_tool_call_id(&model.id);
                let mut function_response = json!({
                    "name": result.tool_name(),
                    "response": if result.is_error() {
                        json!({ "error": response_value })
                    } else {
                        json!({ "output": response_value })
                    },
                });
                if has_images && multimodal_ok {
                    function_response["parts"] = json!(image_parts);
                }
                if include_id {
                    function_response["id"] = json!(result.tool_call_id());
                }
                let function_response_part = json!({ "functionResponse": function_response });

                // Merge consecutive function responses into a single user turn.
                let should_merge = contents
                    .last()
                    .map(|last| {
                        last.get("role").and_then(|r| r.as_str()) == Some("user")
                            && last
                                .get("parts")
                                .and_then(|p| p.as_array())
                                .is_some_and(|parts| {
                                    parts.iter().any(|p| p.get("functionResponse").is_some())
                                })
                    })
                    .unwrap_or(false);
                if should_merge {
                    if let Some(last) = contents.last_mut() {
                        if let Some(parts) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                            parts.push(function_response_part);
                        }
                    }
                } else {
                    contents.push(json!({
                        "role": "user",
                        "parts": [function_response_part],
                    }));
                }

                // Gemini < 3: images go in a separate user message.
                if has_images && !multimodal_ok {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(image_parts);
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            }
        }
    }
    contents
}

const JSON_SCHEMA_META_DECLARATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions",
];

/// Strip meta-declarations from a schema object (upstream
/// `sanitizeForOpenApi`).
pub fn sanitize_for_open_api(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (key, val) in map {
                if JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()) {
                    continue;
                }
                result.insert(key.clone(), sanitize_for_open_api(val));
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// Strip JSON-schema meta-declarations from a tool's parameters (a shared
/// helper for schema tooling).
pub fn sanitize_tool_parameters_for_openapi(parameters: &Value) -> Value {
    sanitize_for_open_api(parameters)
}

/// Convert tools to Gemini function declarations.
pub fn convert_tools(
    tools: &[crate::types::Tool],
    use_parameters: bool,
    supports_strict_mode: bool,
) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let strict = supports_google_strict_tool_sampling_check(tool, supports_strict_mode);
            let parameters = tool_json_schema_parameters(tool, strict);
            let mut decl = json!({
                "name": tool.name,
                "description": tool.description,
            });
            if use_parameters {
                decl["parameters"] = sanitize_for_open_api(&parameters);
            } else {
                decl["parametersJsonSchema"] = parameters;
            }
            decl
        })
        .collect();
    Some(json!([{ "functionDeclarations": declarations }]))
}

/// Resolve strict-sampling preference for a tool (upstream
/// `resolveJsonSchemaStrictSampling`).
fn supports_google_strict_tool_sampling_check(
    tool: &crate::types::Tool,
    supports_strict_mode: bool,
) -> bool {
    match &tool.constrained_sampling {
        Some(crate::types::ConstrainedSampling::JsonSchema { strict }) => {
            supports_strict_mode && matches!(strict, crate::types::StrictPreference::Require)
        }
        _ => false,
    }
}

/// Get JSON-schema tool parameters (upstream `getJsonSchemaToolParameters`).
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

/// Gemini 3+ enforces required function parameters in validated modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    get_gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

/// Map tool choice string to Gemini FunctionCallingConfig mode.
pub fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "auto" => "AUTO",
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// Resolve the tool calling mode (upstream `resolveGoogleFunctionCallingMode`).
pub fn resolve_google_function_calling_mode(
    tools: &[crate::types::Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Option<String> {
    let use_strict_mode = tools.iter().any(|tool| {
        matches!(
            tool.constrained_sampling,
            Some(crate::types::ConstrainedSampling::JsonSchema {
                strict: crate::types::StrictPreference::Require,
            })
        ) && supports_strict_mode
    });
    if matches!(tool_choice, Some("none") | Some("any")) {
        return Some(map_tool_choice(tool_choice.unwrap()).to_string());
    }
    if use_strict_mode {
        return Some("VALIDATED".to_string());
    }
    tool_choice.map(|c| map_tool_choice(c).to_string())
}

/// Map Gemini FinishReason to the unified StopReason.
pub fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("STOP") => StopReason::Stop,
        Some("MAX_TOKENS") => StopReason::Length,
        Some(_) => StopReason::Error,
        None => StopReason::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use crate::types::*;

    fn model(id: &str) -> Model {
        Model::new(id, id, "google-generative-ai", "google")
    }

    #[test]
    fn requires_tool_call_id_gemini3() {
        assert!(requires_tool_call_id("gemini-3-pro"));
        assert!(requires_tool_call_id("gemini-3.1-pro-preview"));
        assert!(requires_tool_call_id("claude-sonnet-4-6"));
        assert!(requires_tool_call_id("gpt-oss-120b"));
        assert!(!requires_tool_call_id("gemini-2.5-pro"));
        assert!(!requires_tool_call_id("gemini-flash-latest"));
        assert!(!requires_tool_call_id("gemma-4-27b"));
    }

    #[test]
    fn is_thinking_part_marker() {
        assert!(is_thinking_part(&json!({ "text": "x", "thought": true })));
        assert!(!is_thinking_part(&json!({ "text": "x" })));
        assert!(!is_thinking_part(&json!({ "text": "x", "thought": false })));
    }

    #[test]
    fn retain_signature_keeps_last_nonempty() {
        assert_eq!(retain_thought_signature(Some("a"), Some("b")), Some("b".into()));
        assert_eq!(retain_thought_signature(Some("a"), None), Some("a".into()));
        assert_eq!(retain_thought_signature(None, Some("")), None);
    }

    #[test]
    fn resolve_maps_pi_levels() {
        let m = model("gemini-2.5-pro");
        let r = resolve_google_thinking_level(ModelThinkingLevel::Medium, &m);
        assert_eq!(r, ResolvedGoogleThinkingLevel::Medium);
        let r = resolve_google_thinking_level(ModelThinkingLevel::Off, &m);
        assert_eq!(r, ResolvedGoogleThinkingLevel::High);
    }

    #[test]
    fn resolve_uses_model_level_map() {
        let mut m = model("claude-sonnet-4-6");
        let mut map = ThinkingLevelMap::new();
        map.insert(ModelThinkingLevel::Low, Some("HIGH".to_string()));
        m.thinking_level_map = Some(map);
        let r = resolve_google_thinking_level(ModelThinkingLevel::Low, &m);
        assert_eq!(r, ResolvedGoogleThinkingLevel::High);
    }

    #[test]
    fn convert_messages_basic_roundtrip() {
        let m = model("gemini-2.5-pro");
        let mut ctx = Context::default();
        ctx.messages = vec![
            Message::User(UserContent::string("hello", 1)),
            Message::Assistant(assistant_with_text("hi there")),
        ];
        let contents = convert_messages(&m, &ctx);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hello");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "hi there");
    }

    fn assistant_with_text(s: &str) -> AssistantMessage {
        let mut a = AssistantMessage::new();
        *a.content_mut() = vec![ContentBlock::text(s)];
        a.set_api_provider_model("google-generative-ai", "google", "gemini-2.5-pro");
        a
    }

    #[test]
    fn convert_messages_tool_result_merges_and_images() {
        let m = model("gemini-2.5-pro"); // < 3 -> images separate
        let mut ctx = Context::default();
        ctx.messages = vec![
            Message::ToolResult(ToolResultMessage::new(
                "call_1",
                "bash",
                vec![ContentBlock::text("out"), ContentBlock::image("aGk=", "image/png")],
                false,
            )),
        ];
        let contents = convert_messages(&m, &ctx);
        // Function response user turn + separate image user turn for <3.
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["parts"][0]["functionResponse"]["name"], "bash");
        assert_eq!(contents[0]["parts"][0]["functionResponse"]["response"]["output"], "out");
        assert_eq!(contents[1]["parts"][1]["inlineData"]["data"], "aGk=");
    }

    #[test]
    fn convert_tools_basic_and_strict() {
        let tools = vec![crate::types::json_tool(
            "bash",
            "run a command",
            &json!({ "type": "object", "properties": {} }),
        )];
        let out = convert_tools(&tools, false, true).unwrap();
        assert_eq!(out[0]["functionDeclarations"][0]["name"], "bash");
        assert!(out[0]["functionDeclarations"][0]["parametersJsonSchema"].is_object());
    }

    #[test]
    fn map_stop_reason_cases() {
        assert_eq!(map_stop_reason(Some("STOP")), StopReason::Stop);
        assert_eq!(map_stop_reason(Some("MAX_TOKENS")), StopReason::Length);
        assert_eq!(map_stop_reason(Some("SAFETY")), StopReason::Error);
        assert_eq!(map_stop_reason(None), StopReason::Pending);
    }
}
