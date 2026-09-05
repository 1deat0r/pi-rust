//! Mistral Conversations API adaptor — port of
//! `packages/ai/src/api/mistral-conversations.ts`.
//!
//! Streams the native Mistral Chat Completions endpoint
//! (`POST /v1/chat/completions`) with SDK-style camelCase payloads converted
//! to the Mistral wire format (snake_case), incremental SSE consumption,
//! thinking/text/tool-call deltas, cached-token usage tracking, and the
//! upstream stop-reason / error surfaces. `stream` never throws: failures are
//! encoded as a terminal error event.
//!
//! Divergences (documented):
//! - `sanitizeSurrogates` is a no-op: Rust strings are always valid UTF-8 and
//!   can never contain lone surrogate halves.

use std::collections::BTreeMap;
use std::error::Error;

use serde_json::{json, Value};

use futures_util::StreamExt;

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{calculate_cost, clamp_thinking_level, Model};
use crate::sse::{SseEvent, SseParser};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, Message,
    ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, Tool, ToolChoice, Usage,
};

use super::openai_completions::short_hash;
use super::openai_completions::{
    abortable, apply_payload_hook, error_reason, format_reqwest_error, immediate_error_stream,
    signal_aborted, terminal_error_message,
};
use super::transform_messages::transform_messages;

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;
/// Default request timeout (upstream `AbortSignal.timeout(options?.timeoutMs ?? 60_000)`).
const DEFAULT_MISTRAL_TIMEOUT_MS: u64 = 60_000;

/// Provider-specific options for the Mistral API (upstream `MistralOptions`).
#[derive(Clone, Default)]
pub struct MistralOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<Value>,
    pub prompt_mode: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// The upstream pi user-agent header value (`getPiUserAgent`). The runtime OS
/// string doesn't attempt to mirror Node's `os.platform()`/`os.release()`, but
/// keeps the same stable `pi (...)` shape.
pub(crate) fn pi_user_agent() -> String {
    format!("pi ({}; {})", std::env::consts::OS, std::env::consts::ARCH)
}

/// Resolve the Mistral chat-completions URL from a model base URL (upstream
/// `new URL("v1/chat/completions", baseUrl)` with a trailing slash guaranteed
/// on the base path).
fn mistral_chat_url(base_url: &str) -> Result<String, String> {
    let mut url =
        url::Url::parse(base_url).map_err(|e| format!("Invalid Mistral base URL: {e}"))?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/v1/chat/completions"));
    // `new URL("v1/chat/completions", baseUrl)` does not carry query or
    // fragment components from the base URL into the request target.
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Tool call id normalization
// ---------------------------------------------------------------------------

/// Normalizes tool call ids to Mistral's 9-char alphanumeric convention,
/// mapping duplicate ids deterministically (upstream
/// `createMistralToolCallIdNormalizer`).
#[derive(Default)]
struct MistralToolCallIdNormalizer {
    id_map: std::collections::HashMap<String, String>,
    reverse_map: std::collections::HashMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }
        let mut attempt = 0usize;
        let candidate = loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            let owner = self.reverse_map.get(&candidate).cloned();
            match owner {
                None => break candidate,
                Some(owner) if owner == id => break candidate,
                Some(_) => attempt += 1,
            }
        };
        self.id_map.insert(id.to_string(), candidate.clone());
        self.reverse_map.insert(candidate.clone(), id.to_string());
        candidate
    }
}

/// Mirror of upstream `deriveMistralToolCallId`.
fn derive_mistral_tool_call_id(id: &str, attempt: usize) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id.to_string()
    } else {
        normalized
    };
    let seed = if attempt == 0 {
        seed_base.clone()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

// ---------------------------------------------------------------------------
// Strict JSON schema (constrained sampling)
// ---------------------------------------------------------------------------

/// Thrown when a schema cannot be converted to the strict subset expected by
/// provider constrained sampling (upstream `UnsupportedStrictJsonSchemaError`).
#[derive(Debug)]
struct UnsupportedStrictJsonSchema(String);

impl std::fmt::Display for UnsupportedStrictJsonSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UnsupportedStrictJsonSchema {}

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

/// `makeJsonSchemaNodeStrict` port: validates and rewrites a schema node into
/// the strict provider subset (explicit required arrays, `additionalProperties:
/// false`, optional properties wrapped in `anyOf` with `null`).
#[allow(clippy::expect_used)] // invariant: anyOf/items/properties presence checked above each use
fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), UnsupportedStrictJsonSchema> {
    let obj = match schema.as_object_mut() {
        Some(obj) => obj,
        None => {
            return Err(UnsupportedStrictJsonSchema(
                "boolean schemas are unsupported".to_string(),
            ))
        }
    };

    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if obj.contains_key(*key) {
            return Err(UnsupportedStrictJsonSchema(format!(
                "{key} schemas are unsupported"
            )));
        }
    }

    if obj.get("anyOf").is_some() {
        let variants = obj.get("anyOf").and_then(|v| v.as_array());
        match variants {
            Some(variants) if !variants.is_empty() => {
                for variant in variants {
                    if is_structured_schema(variant) {
                        return Err(UnsupportedStrictJsonSchema(
                            "object and array unions are unsupported".to_string(),
                        ));
                    }
                }
            }
            _ => {
                return Err(UnsupportedStrictJsonSchema(
                    "anyOf must contain at least one schema".to_string(),
                ))
            }
        }
        // Recursively strictify each variant.
        let variants = obj
            .get_mut("anyOf")
            .and_then(Value::as_array_mut)
            .expect("anyOf variants validated by caller");
        for variant in variants.iter_mut() {
            make_json_schema_node_strict(variant)?;
        }
    }

    if obj.get("items").is_some() {
        let items = obj.get_mut("items").expect("items presence checked above");
        if items.is_array() {
            return Err(UnsupportedStrictJsonSchema(
                "tuple schemas are unsupported".to_string(),
            ));
        }
        make_json_schema_node_strict(items)?;
    }

    let is_object_schema = obj.get("type").and_then(|t| t.as_str()) == Some("object");
    if obj.contains_key("properties") && !is_object_schema {
        return Err(UnsupportedStrictJsonSchema(
            "properties require type object".to_string(),
        ));
    }
    if !is_object_schema {
        return Ok(());
    }
    if let Some(additional) = obj.get("additionalProperties") {
        if additional != &Value::Bool(false) {
            return Err(UnsupportedStrictJsonSchema(
                "schema-valued or true additionalProperties is unsupported".to_string(),
            ));
        }
    }
    if let Some(properties) = obj.get("properties") {
        if !properties.is_object() {
            return Err(UnsupportedStrictJsonSchema(
                "object properties must be a schema map".to_string(),
            ));
        }
    }
    if let Some(required) = obj.get("required") {
        if !required
            .as_array()
            .is_some_and(|arr| arr.iter().all(|k| k.is_string()))
        {
            return Err(UnsupportedStrictJsonSchema(
                "object required must be a string array".to_string(),
            ));
        }
    }
    let property_names: Vec<String> = obj
        .entry("properties".to_string())
        .or_insert_with(|| json!({}))
        .as_object()
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    let required: std::collections::BTreeSet<String> = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if required.iter().any(|key| !property_names.contains(key)) {
        return Err(UnsupportedStrictJsonSchema(
            "required contains an unknown property".to_string(),
        ));
    }
    for key in &property_names {
        let mut property = obj
            .get("properties")
            .and_then(|p| p.get(key))
            .cloned()
            .unwrap_or(Value::Null);
        make_json_schema_node_strict(&mut property)?;
        if !required.contains(key) && !schema_allows_null(&property) {
            property = json!({ "anyOf": [property, { "type": "null" }] });
        }
        if let Some(properties) = obj.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert(key.clone(), property);
        }
    }
    obj.insert("required".to_string(), json!(property_names));
    obj.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

fn is_structured_schema(schema: &Value) -> bool {
    if !schema.is_object() {
        return false;
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    };
    types.iter().any(|t| *t == "object" || *t == "array")
        || schema.get("properties").is_some()
        || schema.get("items").is_some()
}

fn schema_allows_null(schema: &Value) -> bool {
    if !schema.is_object() {
        return false;
    }
    let type_allows_null = match schema.get("type") {
        Some(Value::String(s)) => s == "null",
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some("null")),
        _ => false,
    };
    if type_allows_null {
        return true;
    }
    if schema.get("const") == Some(&Value::Null) {
        return true;
    }
    if schema
        .get("enum")
        .and_then(|e| e.as_array())
        .is_some_and(|arr| arr.iter().any(|v| v.is_null()))
    {
        return true;
    }
    schema
        .get("anyOf")
        .and_then(|a| a.as_array())
        .is_some_and(|array| array.iter().any(schema_allows_null))
}

/// Port of `makeStrictJsonSchema`: clone a tool parameter schema and rewrite
/// it into the strict provider subset.
pub(crate) fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err("root schema must have type object".to_string());
    }
    make_json_schema_node_strict(&mut cloned).map_err(|e| e.to_string())?;
    if cloned.get("type").and_then(|t| t.as_str()) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

/// Port of `resolveJsonSchemaStrictSampling`: `Some(true)` when the schema is
/// strict-convertible, `None` when strict mode doesn't apply, `Err` when a
/// `require` constraint cannot be honored.
pub(crate) fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let config = match &tool.constrained_sampling {
        Some(crate::types::ConstrainedSampling::JsonSchema { strict }) => strict,
        _ => return Ok(None),
    };
    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(err) => {
                if *config == crate::types::StrictPreference::Require {
                    Err(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {}.",
                        tool.name, err
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    } else if *config == crate::types::StrictPreference::Require {
        Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ))
    } else {
        Ok(None)
    }
}

/// Convert tools to the Mistral function-tool wire shape (upstream
/// `toFunctionTools`).
fn to_function_tools(tools: &[Tool]) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, true)?;
        let parameters = if strict == Some(true) {
            make_strict_json_schema(&tool.parameters)?
        } else {
            tool.parameters.clone()
        };
        result.push(json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
                "strict": strict.unwrap_or(false),
            }
        }));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            Message::User(user) => match user.content() {
                crate::types::UserContentBody::String(s) => {
                    result.push(json!({ "role": "user", "content": s }));
                }
                crate::types::UserContentBody::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|item| matches!(item, ContentBlock::Image { .. }));
                    let mut content: Vec<Value> = Vec::new();
                    for item in blocks {
                        match item {
                            ContentBlock::Text { text, .. } => {
                                content.push(json!({ "type": "text", "text": text }));
                            }
                            ContentBlock::Image {
                                data, mime_type, ..
                            } if supports_images => {
                                content.push(json!({
                                    "type": "image_url",
                                    "imageUrl": format!("data:{mime_type};base64,{data}"),
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                        continue;
                    }
                    if had_images && !supports_images {
                        result.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut content_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in assistant.content() {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if !text.trim().is_empty() {
                                content_parts.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            if !thinking.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "thinking",
                                    "thinking": [{ "type": "text", "text": thinking }],
                                }));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            let args = serde_json::to_string(&arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": args },
                                "index": 0,
                            }));
                        }
                        _ => {}
                    }
                }
                let mut assistant_message = json!({ "role": "assistant", "prefix": false });
                if !content_parts.is_empty() {
                    assistant_message["content"] = json!(content_parts);
                }
                if !tool_calls.is_empty() {
                    assistant_message["toolCalls"] = json!(tool_calls);
                }
                if !content_parts.is_empty() || !tool_calls.is_empty() {
                    result.push(assistant_message);
                }
            }
            Message::ToolResult(tool_result) => {
                let text_result: String = tool_result
                    .content()
                    .iter()
                    .filter_map(|part| match part {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content()
                    .iter()
                    .any(|part| matches!(part, ContentBlock::Image { .. }));
                let tool_text = build_tool_result_text(
                    &text_result,
                    has_images,
                    supports_images,
                    tool_result.is_error(),
                );
                let mut tool_content: Vec<Value> =
                    vec![json!({ "type": "text", "text": tool_text })];
                for part in tool_result.content() {
                    if let ContentBlock::Image {
                        data, mime_type, ..
                    } = part
                    {
                        if supports_images {
                            tool_content.push(json!({
                                "type": "image_url",
                                "imageUrl": format!("data:{mime_type};base64,{data}"),
                            }));
                        }
                    }
                }
                result.push(json!({
                    "role": "tool",
                    "toolCallId": tool_result.tool_call_id(),
                    "name": tool_result.tool_name(),
                    "content": tool_content,
                }));
            }
        }
    }
    result
}

fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }
    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".to_string()
            } else {
                "(see attached image)".to_string()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".to_string()
        } else {
            "(image omitted: model does not support images)".to_string()
        };
    }
    if is_error {
        "[tool error] (no tool output)".to_string()
    } else {
        "(no tool output)".to_string()
    }
}

// ---------------------------------------------------------------------------
// Payload building
// ---------------------------------------------------------------------------

/// SDK-style (camelCase) chat payload — port of `buildChatPayload`.
fn build_chat_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: &MistralOptions,
) -> Result<Value, String> {
    let supports_images = model.input.contains(&crate::model::ModelInput::Image);
    let mut payload = json!({
        "model": model.id,
        "stream": true,
        "messages": to_chat_messages(messages, supports_images),
    });

    if !context.tools.is_empty() {
        payload["tools"] = json!(to_function_tools(&context.tools)?);
    }
    if let Some(temperature) = options.base.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.base.max_tokens {
        payload["maxTokens"] = json!(max_tokens);
    }
    if let Some(tool_choice) = &options.tool_choice {
        payload["toolChoice"] = map_tool_choice(tool_choice);
    }
    if let Some(prompt_mode) = &options.prompt_mode {
        payload["promptMode"] = json!(prompt_mode);
    }
    if let Some(reasoning_effort) = &options.reasoning_effort {
        payload["reasoningEffort"] = json!(reasoning_effort);
    }
    if should_use_prompt_caching(options) {
        payload["promptCacheKey"] = json!(options.base.session_id.clone().unwrap_or_default());
    }

    if let Some(system_prompt) = &context.system_prompt {
        let mut messages = payload["messages"].as_array().cloned().unwrap_or_default();
        messages.insert(0, json!({ "role": "system", "content": system_prompt }));
        payload["messages"] = json!(messages);
    }

    Ok(payload)
}

fn map_tool_choice(choice: &Value) -> Value {
    // Pass through the accepted string union or the object form unchanged.
    match choice {
        Value::String(s) if s == "auto" || s == "none" || s == "any" || s == "required" => {
            choice.clone()
        }
        Value::Object(_) => choice.clone(),
        _ => Value::Null,
    }
}

fn should_use_prompt_caching(options: &MistralOptions) -> bool {
    options.base.cache_retention.as_deref() != Some(crate::types::CACHE_RETENTION_NONE)
        && options
            .base
            .session_id
            .as_deref()
            .is_some_and(|session_id| !session_id.is_empty())
}

/// Convert an SDK-style payload to the Mistral wire format (snake_case).
/// Mirrors upstream `toMistralWirePayload` (which mutates one object in place;
/// the port clones so on_payload observes the SDK-shaped payload before wire
/// conversion).
fn to_mistral_wire_payload(payload: &Value) -> Value {
    let mut wire = payload.clone();
    if let Some(obj) = wire.as_object_mut() {
        for (source, target) in [
            ("topP", "top_p"),
            ("maxTokens", "max_tokens"),
            ("randomSeed", "random_seed"),
            ("responseFormat", "response_format"),
            ("toolChoice", "tool_choice"),
            ("presencePenalty", "presence_penalty"),
            ("frequencyPenalty", "frequency_penalty"),
            ("parallelToolCalls", "parallel_tool_calls"),
            ("reasoningEffort", "reasoning_effort"),
            ("promptMode", "prompt_mode"),
            ("promptCacheKey", "prompt_cache_key"),
            ("safePrompt", "safe_prompt"),
        ] {
            remap_property(obj, source, target);
        }

        if let Some(response_format) = obj.get_mut("response_format") {
            if response_format.is_object() {
                let mut rf = response_format.take();
                if let Some(rf_obj) = rf.as_object_mut() {
                    remap_property(rf_obj, "jsonSchema", "json_schema");
                }
                if let Some(jso) = rf.get_mut("json_schema") {
                    if let Some(jso_obj) = jso.as_object_mut() {
                        remap_property(jso_obj, "schemaDefinition", "schema");
                    }
                }
                obj.insert("response_format".to_string(), rf);
            }
        }

        if let Some(messages) = obj.get_mut("messages") {
            if let Some(arr) = messages.as_array_mut() {
                for message in arr.iter_mut() {
                    if let Some(msg_obj) = message.as_object_mut() {
                        remap_property(msg_obj, "toolCalls", "tool_calls");
                        remap_property(msg_obj, "toolCallId", "tool_call_id");
                        if let Some(content) = msg_obj.get_mut("content") {
                            if let Some(parts) = content.as_array_mut() {
                                for part in parts.iter_mut() {
                                    if let Some(part_obj) = part.as_object_mut() {
                                        for (source, target) in [
                                            ("imageUrl", "image_url"),
                                            ("documentUrl", "document_url"),
                                            ("documentName", "document_name"),
                                            ("fileId", "file_id"),
                                            ("referenceIds", "reference_ids"),
                                            ("inputAudio", "input_audio"),
                                        ] {
                                            remap_property(part_obj, source, target);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    wire
}

fn remap_property(obj: &mut serde_json::Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = obj.remove(source) {
        obj.insert(target.to_string(), value);
    }
}

// ---------------------------------------------------------------------------
// SSE reading
// ---------------------------------------------------------------------------

/// Read and consume the response body as SSE events until `[DONE]` or EOF
/// (upstream `readMistralEvents`). Events are handed to the consumer as soon
/// as the parser completes each SSE frame so text/tool deltas are observable
/// while the response is still in flight.
async fn read_mistral_events(
    response: reqwest::Response,
    signal: Option<crate::types::AbortSignal>,
    state: &mut MistralStreamState,
    output: &mut AssistantMessage,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
    model: &Model,
) -> Result<(), String> {
    let mut parser = SseParser::new();
    let mut stream = response.bytes_stream();

    loop {
        let next = abortable(stream.next(), signal.clone())
            .await
            .map_err(|_| "Request was aborted".to_string())?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|error| {
            format!(
                "Mistral stream read failed: {}",
                format_reqwest_error(&error)
            )
        })?;
        for event in parser.push_bytes(&chunk) {
            if event.data.trim() == "[DONE]" {
                return Ok(()); // upstream returns on MISTRAL_STREAM_DONE
            }
            consume_chat_stream_into(std::slice::from_ref(&event), state, output, push, model)?;
        }
    }
    for event in parser.finish() {
        if event.data.trim() == "[DONE]" {
            break;
        }
        consume_chat_stream_into(std::slice::from_ref(&event), state, output, push, model)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

/// Extract the cached-token count from a Mistral usage object, checking every
/// casing/placement the API has sent (upstream `getMistralCachedPromptTokens`).
fn get_mistral_cached_prompt_tokens(usage: &Value, prompt_tokens: i64) -> i64 {
    let cached_value = [
        usage
            .get("promptTokensDetails")
            .and_then(|v| v.get("cachedTokens")),
        usage
            .get("prompt_tokens_details")
            .and_then(|v| v.get("cached_tokens")),
        usage
            .get("promptTokenDetails")
            .and_then(|v| v.get("cachedTokens")),
        usage
            .get("prompt_token_details")
            .and_then(|v| v.get("cached_tokens")),
        usage.get("numCachedTokens"),
        usage.get("num_cached_tokens"),
    ]
    .into_iter()
    .flatten()
    // JavaScript's nullish-coalescing chain selects the first non-null
    // property, even when that property has the wrong type.
    .find(|value| !value.is_null())
    .and_then(|value| value.as_f64())
    .filter(|f| f.is_finite() && *f >= 0.0)
    .map(|f| f as i64)
    .unwrap_or(0);
    prompt_tokens.min(cached_value)
}

fn map_chat_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        "error" => (
            StopReason::Error,
            Some("Provider stopped with: error".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

#[derive(Clone, Copy, PartialEq)]
enum MistralBlockKind {
    Text,
    Thinking,
}

#[derive(Default)]
struct MistralStreamState {
    current_block: Option<(usize, MistralBlockKind)>,
    tool_blocks_by_key: std::collections::HashMap<String, usize>,
    tool_block_order: Vec<String>,
    // Streaming scratch buffers for partial tool-call arguments (upstream
    // `partialArgs` on the tool blocks; never persisted).
    partial_args: std::collections::HashMap<String, String>,
}

fn finish_current_block(
    current_block: Option<(usize, MistralBlockKind)>,
    output: &AssistantMessage,
    push: &mut dyn FnMut(AssistantMessageEvent),
) {
    let Some((idx, kind)) = current_block else {
        return;
    };
    match (kind, &output.content()[idx]) {
        (MistralBlockKind::Text, ContentBlock::Text { text, .. }) => {
            push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content: text.clone(),
                partial: output.clone(),
            });
        }
        (MistralBlockKind::Thinking, ContentBlock::Thinking { thinking, .. }) => {
            push(AssistantMessageEvent::ThinkingEnd {
                content_index: idx,
                content: thinking.clone(),
                partial: output.clone(),
            });
        }
        _ => {}
    }
}

/// Consume a parsed Mistral chat-completion event stream into the unified
/// assistant message + event protocol (upstream `consumeChatStream`).
#[cfg(test)]
fn consume_chat_stream(
    events: &[SseEvent],
    output: &mut AssistantMessage,
    push: &mut dyn FnMut(AssistantMessageEvent),
    model: &Model,
) -> Result<(), String> {
    let mut state = MistralStreamState::default();
    consume_chat_stream_into(events, &mut state, output, push, model)?;
    finish_mistral_stream(&mut state, output, push);
    Ok(())
}

#[allow(clippy::expect_used)] // invariants checked immediately above each use
fn consume_chat_stream_into(
    events: &[SseEvent],
    state: &mut MistralStreamState,
    output: &mut AssistantMessage,
    push: &mut dyn FnMut(AssistantMessageEvent),
    model: &Model,
) -> Result<(), String> {
    for event in events {
        if event.data.trim().is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data)
            .map_err(|e| format!("Invalid Mistral streaming event: {e}"))?;
        if !chunk.is_object() || chunk.get("choices").and_then(|v| v.as_array()).is_none() {
            return Err("Invalid Mistral streaming event".to_string());
        }
        let choices = chunk
            .get("choices")
            .and_then(|v| v.as_array())
            .expect("choices presence checked above");

        // The streamed CompletionChunk carries an id field; keep the first
        // non-empty one (upstream `output.responseId ||= chunk.id`).
        if output.response_id().is_none() {
            if let Some(id) = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                output.set_response_id(id.to_string());
            }
        }

        if let Some(usage) = chunk.get("usage") {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cached_prompt_tokens = get_mistral_cached_prompt_tokens(usage, prompt_tokens);
            let completion_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let total_tokens = usage
                .get("total_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let mut u = Usage {
                input: prompt_tokens.saturating_sub(cached_prompt_tokens),
                output: completion_tokens,
                cache_read: cached_prompt_tokens,
                cache_write: 0,
                total_tokens,
                cache_write_1h: None,
                reasoning: None,
                cost: Default::default(),
            };
            if u.total_tokens == 0 {
                u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
            }
            let cost = calculate_cost(model, &u);
            u.cost = cost;
            output.set_usage(u);
        }

        let Some(choice) = choices.first().filter(|c| c.is_object()) else {
            continue;
        };

        if let Some(finish_reason) = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|reason| !reason.is_empty())
        {
            output.set_raw_stop_reason(finish_reason.to_string());
            let (stop_reason, error_message) = map_chat_stop_reason(finish_reason);
            output.set_stop_reason(stop_reason);
            if let Some(err) = error_message {
                let AssistantMessage::Assistant { error_message, .. } = output;
                *error_message = Some(err);
            }
        }

        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(content) = delta.get("content") {
            if !content.is_null() {
                let content_items: Vec<Value> = if let Some(s) = content.as_str() {
                    vec![json!(s)]
                } else if let Some(arr) = content.as_array() {
                    arr.clone()
                } else {
                    Vec::new()
                };
                for item in content_items {
                    if let Some(text_delta) = item.as_str() {
                        if !matches!(state.current_block, Some((_, MistralBlockKind::Text))) {
                            finish_current_block(state.current_block, output, push);
                            output.content_mut().push(ContentBlock::text(""));
                            let idx = output.content_len() - 1;
                            state.current_block = Some((idx, MistralBlockKind::Text));
                            push(AssistantMessageEvent::TextStart {
                                content_index: idx,
                                partial: output.clone(),
                            });
                        }
                        let idx = state
                            .current_block
                            .expect("stream block invariant: open block tracked")
                            .0;
                        if let Some(ContentBlock::Text { text, .. }) =
                            output.content_mut().get_mut(idx)
                        {
                            *text += text_delta;
                        }
                        push(AssistantMessageEvent::TextDelta {
                            content_index: idx,
                            delta: text_delta.to_string(),
                            partial: output.clone(),
                        });
                        continue;
                    }
                    if !item.is_object() {
                        continue;
                    }
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "thinking" {
                        let delta_text: String = item
                            .get("thinking")
                            .and_then(|t| t.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        if delta_text.is_empty() {
                            continue;
                        }
                        if !matches!(state.current_block, Some((_, MistralBlockKind::Thinking))) {
                            finish_current_block(state.current_block, output, push);
                            output.content_mut().push(ContentBlock::thinking(""));
                            let idx = output.content_len() - 1;
                            state.current_block = Some((idx, MistralBlockKind::Thinking));
                            push(AssistantMessageEvent::ThinkingStart {
                                content_index: idx,
                                partial: output.clone(),
                            });
                        }
                        let idx = state
                            .current_block
                            .expect("stream block invariant: open block tracked")
                            .0;
                        if let Some(ContentBlock::Thinking { thinking, .. }) =
                            output.content_mut().get_mut(idx)
                        {
                            *thinking += &delta_text;
                        }
                        push(AssistantMessageEvent::ThinkingDelta {
                            content_index: idx,
                            delta: delta_text,
                            partial: output.clone(),
                        });
                        continue;
                    }
                    if item_type == "text" {
                        let text_delta = item
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !matches!(state.current_block, Some((_, MistralBlockKind::Text))) {
                            finish_current_block(state.current_block, output, push);
                            output.content_mut().push(ContentBlock::text(""));
                            let idx = output.content_len() - 1;
                            state.current_block = Some((idx, MistralBlockKind::Text));
                            push(AssistantMessageEvent::TextStart {
                                content_index: idx,
                                partial: output.clone(),
                            });
                        }
                        let idx = state
                            .current_block
                            .expect("stream block invariant: open block tracked")
                            .0;
                        if let Some(ContentBlock::Text { text, .. }) =
                            output.content_mut().get_mut(idx)
                        {
                            *text += &text_delta;
                        }
                        push(AssistantMessageEvent::TextDelta {
                            content_index: idx,
                            delta: text_delta,
                            partial: output.clone(),
                        });
                    }
                }
            }
        }

        let tool_calls = delta
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for tool_call in tool_calls {
            if state.current_block.is_some() {
                finish_current_block(state.current_block, output, push);
                state.current_block = None;
            }
            let index = tool_call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let call_id = tool_call
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && *s != "null")
                .map(|s| s.to_string())
                .unwrap_or_else(|| derive_mistral_tool_call_id(&format!("toolcall:{index}"), 0));
            let key = format!("{call_id}:{index}");

            let block_index = match state.tool_blocks_by_key.get(&key).copied() {
                Some(idx) => idx,
                None => {
                    let name = tool_call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    output.content_mut().push(ContentBlock::tool_call(
                        call_id.clone(),
                        name,
                        json!({}),
                    ));
                    let idx = output.content_len() - 1;
                    state.tool_blocks_by_key.insert(key.clone(), idx);
                    state.tool_block_order.push(key.clone());
                    push(AssistantMessageEvent::ToolCallStart {
                        content_index: idx,
                        partial: output.clone(),
                    });
                    idx
                }
            };

            let args_delta = match tool_call.get("function").and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "{}".to_string(),
                Some(Value::Bool(false)) => "{}".to_string(),
                Some(Value::Number(number)) if number.as_f64() == Some(0.0) => "{}".to_string(),
                Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
            };
            let accumulated = state.partial_args.entry(key.clone()).or_default();
            accumulated.push_str(&args_delta);
            let parsed = crate::partial_json::parse_streaming_json(accumulated);
            if let Some(ContentBlock::ToolCall { arguments, .. }) =
                output.content_mut().get_mut(block_index)
            {
                *arguments = parsed;
            }
            push(AssistantMessageEvent::ToolCallDelta {
                content_index: block_index,
                delta: args_delta,
                partial: output.clone(),
            });
        }
    }
    Ok(())
}

fn finish_mistral_stream(
    state: &mut MistralStreamState,
    output: &mut AssistantMessage,
    push: &mut dyn FnMut(AssistantMessageEvent),
) {
    finish_current_block(state.current_block, output, push);
    state.current_block = None;
    for key in &state.tool_block_order {
        let index = state.tool_blocks_by_key[key];
        let final_args = state.partial_args.get(key).cloned().unwrap_or_default();
        let parsed = crate::partial_json::parse_streaming_json(&final_args);
        if let Some(ContentBlock::ToolCall { arguments, .. }) = output.content_mut().get_mut(index)
        {
            *arguments = parsed;
        }
        let tool_call = output.content()[index].clone();
        push(AssistantMessageEvent::ToolCallEnd {
            content_index: index,
            tool_call,
            partial: output.clone(),
        });
    }
}

// ---------------------------------------------------------------------------
// Error formatting
// ---------------------------------------------------------------------------

fn truncate_error_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!(
        "{truncated}... [truncated {} chars]",
        text.chars().count() - max_chars
    )
}

/// Port of `formatMistralError` for the transport surfaces that carry
/// status/body (the network-request wrappers already build the string).
/// Format a reqwest transport error with its full source chain (so "operation
/// timed out" surfaces rather than the elided top-level message).
fn format_transport_error(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(s) = source {
        text.push_str(&format!(": {s}"));
        source = s.source();
    }
    text
}

fn format_mistral_error(error: &str, status_code: Option<u16>, body: Option<&str>) -> String {
    if let Some(status_code) = status_code {
        if let Some(body_text) = body.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return format!(
                "Mistral API error ({status_code}): {}",
                truncate_error_text(body_text, MAX_MISTRAL_ERROR_BODY_CHARS)
            );
        }
        return format!("Mistral API error ({status_code}): {error}");
    }
    error.to_string()
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Build the final Mistral request headers as a (lowercased name, value) map
/// (upstream `buildMistralHeaders`). Defaults are set first, then model static
/// headers, then per-request headers — both override case-insensitively, and a
/// `null` per-request value deletes the header. The `x-affinity` session
/// header is added only when prompt caching is active and no explicit affinity
/// override exists (upstream).
fn build_mistral_headers(
    model: &Model,
    api_key: &str,
    base: &StreamOptions,
) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("user-agent".to_string(), pi_user_agent());
    headers.insert("accept".to_string(), "text/event-stream".to_string());
    headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
    headers.insert("content-type".to_string(), "application/json".to_string());

    if let Some(model_headers) = &model.headers {
        // Model static headers always have string values (upstream
        // `applyMistralHeaderOverrides` with `Record<string, string>`).
        for (name, value) in model_headers {
            headers.insert(name.to_lowercase(), value.clone());
        }
    }
    apply_header_overrides(&mut headers, base.base.headers.as_ref());

    let has_explicit_affinity = has_model_header_override(model, "x-affinity")
        || has_header_override(base.base.headers.as_ref(), "x-affinity");
    if should_use_prompt_cache(base) && !has_explicit_affinity {
        if let Some(session_id) = &base.session_id {
            headers.insert("x-affinity".to_string(), session_id.clone());
        }
    }
    headers
}

fn apply_header_overrides(
    headers: &mut BTreeMap<String, String>,
    overrides: Option<&crate::types::ProviderHeaders>,
) {
    let Some(overrides) = overrides else { return };
    for (name, value) in overrides {
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

fn has_header_override(overrides: Option<&crate::types::ProviderHeaders>, target: &str) -> bool {
    overrides.is_some_and(|o| o.keys().any(|name| name.to_lowercase() == target))
}

fn has_model_header_override(model: &Model, target: &str) -> bool {
    model
        .headers
        .as_ref()
        .is_some_and(|o| o.keys().any(|name| name.to_lowercase() == target))
}

fn should_use_prompt_cache(base: &StreamOptions) -> bool {
    base.cache_retention.as_deref() != Some(crate::types::CACHE_RETENTION_NONE)
        && base
            .session_id
            .as_deref()
            .is_some_and(|session_id| !session_id.is_empty())
}

// ---------------------------------------------------------------------------
// Reasoning controls
// ---------------------------------------------------------------------------

/// Model ids that use `reasoning_effort` (upstream `usesReasoningEffort`).
fn uses_reasoning_effort(model: &Model) -> bool {
    matches!(
        model.id.as_str(),
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

/// Models with native reasoning use `prompt_mode` (upstream
/// `usesPromptModeReasoning`).
fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

/// Map a clamped thinking level to Mistral's `reasoning_effort` value
/// (upstream `mapReasoningEffort`).
fn map_reasoning_effort(model: &Model, level: ModelThinkingLevel) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&level))
        .cloned()
        .flatten()
        .unwrap_or_else(|| "high".to_string())
}

/// Resolve the reasoning controls for `streamSimple` (upstream
/// `streamSimple`) into `(prompt_mode, reasoning_effort)`.
#[allow(clippy::expect_used)] // caller passes Some reasoning only
fn resolve_reasoning_controls(
    model: &Model,
    options: &SimpleStreamOptions,
) -> (Option<String>, Option<String>) {
    let reasoning = options
        .reasoning
        .map(|r| clamp_thinking_level(model, ModelThinkingLevel::from(r)));
    let should_use_reasoning =
        model.reasoning && reasoning.is_some() && reasoning != Some(ModelThinkingLevel::Off);
    let prompt_mode = if should_use_reasoning && uses_prompt_mode_reasoning(model) {
        Some("reasoning".to_string())
    } else {
        None
    };
    let reasoning_effort = if should_use_reasoning && uses_reasoning_effort(model) {
        Some(map_reasoning_effort(
            model,
            reasoning.expect("reasoning required by caller"),
        ))
    } else {
        None
    };
    (prompt_mode, reasoning_effort)
}

// ---------------------------------------------------------------------------
// Main stream functions
// ---------------------------------------------------------------------------

async fn run_stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: &str,
    options: &MistralOptions,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
) -> Result<AssistantMessage, String> {
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return Err("Request was aborted".to_string());
    }
    // Normalize tool call ids (upstream normalizer passed into
    // transformMessages; the fixture-driven path pre-normalizes instead).
    let mut output = new_output(model);

    let transformed = {
        let normalizer = std::cell::RefCell::new(MistralToolCallIdNormalizer::default());
        let normalize = |id: &str, _model: &Model, _source: &AssistantMessage| -> String {
            normalizer.borrow_mut().normalize(id)
        };
        transform_messages(&context.messages, model, Some(&normalize))
    };

    let payload = build_chat_payload(model, context, &transformed, options)?;
    let payload = apply_payload_hook(
        payload,
        model,
        options.base.on_payload.as_ref(),
        options.base.abort_signal.clone(),
    )
    .await
    .map_err(|_| "Request was aborted".to_string())?;
    let wire_payload = to_mistral_wire_payload(&payload);
    let url = mistral_chat_url(&model.base_url)?;
    let headers = build_mistral_headers(model, api_key, &options.base);

    let timeout_ms = options
        .base
        .base
        .timeout_ms
        .unwrap_or(DEFAULT_MISTRAL_TIMEOUT_MS);
    let mut request = client
        .post(&url)
        .timeout(std::time::Duration::from_millis(timeout_ms));
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
        request = request.headers(header_map);
    }
    request = request.json(&wire_payload);

    let response = match abortable(request.send(), options.base.abort_signal.clone()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            // Fetch-level failure (DNS, timeout, connection reset, ...).
            return Err(format_mistral_error(
                &format_transport_error(&err),
                None,
                None,
            ));
        }
        Err(_) => return Err("Request was aborted".to_string()),
    };
    let status = response.status();
    let provider_headers: BTreeMap<String, String> = response
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
        headers: provider_headers,
    };
    if let Some(on_response) = &options.base.on_response {
        on_response(&provider_response, model);
    }

    if !status.is_success() {
        let body = abortable(response.text(), options.base.abort_signal.clone())
            .await
            .map_err(|_| "Request was aborted".to_string())?
            .unwrap_or_default();
        let status_text = status.canonical_reason().unwrap_or("").to_string();
        return Err(format_mistral_error(
            if status_text.is_empty() {
                "Request failed"
            } else {
                status_text.as_str()
            },
            Some(status.as_u16()),
            Some(&body),
        ));
    }

    // Fetch exposes a successful response without a body as `response.body
    // === null`; preserve that distinct upstream error before emitting the
    // stream start event. A chunked response remains eligible for normal
    // incremental consumption because its content length is unknown.
    if response.content_length() == Some(0) {
        return Err("Mistral response has no body".to_string());
    }

    push(AssistantMessageEvent::Start {
        partial: new_output(model),
    });

    let mut state = MistralStreamState::default();
    read_mistral_events(
        response,
        options.base.abort_signal.clone(),
        &mut state,
        &mut output,
        push,
        model,
    )
    .await?;
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return Err("Request was aborted".to_string());
    }
    finish_mistral_stream(&mut state, &mut output, push);
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason() == Some(StopReason::Pending) {
        return Err("Mistral stream ended without a finish reason".to_string());
    }
    if output.stop_reason() == Some(StopReason::Aborted)
        || output.stop_reason() == Some(StopReason::Error)
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

/// Stream a request against the Mistral Chat Completions endpoint
/// (upstream `stream`).
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &MistralOptions,
) -> AssistantMessageEventStream {
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return immediate_error_stream(model, "Request was aborted", true);
    }
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
                let aborted = signal_aborted(options.base.abort_signal.as_ref());
                let message = terminal_error_message(
                    &model,
                    if aborted {
                        "Request was aborted".to_string()
                    } else {
                        error_message
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

/// Simple (provider-neutral) `streamSimple` — resolves reasoning controls and
/// forwards (upstream `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let (prompt_mode, reasoning_effort) = resolve_reasoning_controls(model, options);
    let go = MistralOptions {
        base: options.base.clone(),
        tool_choice: options.tool_choice.map(|t| match t {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
        }),
        prompt_mode,
        reasoning_effort,
    };
    stream(model, context, client, api_key, &go)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::ModelInput;
    use crate::types::{json_tool, ContentBlock, Message, ToolResultMessage, UserContent};

    fn mistral_model(id: &str) -> Model {
        let models = crate::providers::catalog_models("mistral");
        models.into_iter().find(|m| m.id == id).unwrap_or_else(|| {
            let mut m = Model::new(id, id, "mistral-conversations", "mistral");
            m.base_url = "https://api.mistral.ai".to_string();
            m.reasoning = true;
            m.input = vec![ModelInput::Text, ModelInput::Image];
            m
        })
    }

    fn sse(data: &str) -> SseEvent {
        SseEvent {
            data: data.to_string(),
            event: None,
            id: None,
        }
    }

    // ------------------------------------------------------------------
    // Payload building / wire format
    // ------------------------------------------------------------------

    #[test]
    fn chat_url_joins_v1_chat_completions() {
        assert_eq!(
            mistral_chat_url("https://api.mistral.ai").unwrap(),
            "https://api.mistral.ai/v1/chat/completions"
        );
        assert_eq!(
            mistral_chat_url("https://api.mistral.ai/").unwrap(),
            "https://api.mistral.ai/v1/chat/completions"
        );
        assert_eq!(
            mistral_chat_url("https://api.mistral.ai/api/?token=ignored#fragment").unwrap(),
            "https://api.mistral.ai/api/v1/chat/completions"
        );
        assert!(mistral_chat_url("not a url").is_err());
    }

    #[test]
    fn wire_payload_remaps_snake_case_and_nested_schema() {
        // Port of mistral-http-transport.test.ts "serializes SDK-style payloads".
        let mut payload = json!({
            "model": "mistral-large-latest",
            "stream": true,
            "maxTokens": 123,
            "promptMode": "reasoning",
            "reasoningEffort": "high",
            "toolChoice": { "type": "function", "function": { "name": "lookup" } },
            "promptCacheKey": "session-1",
            "topP": 0.9,
            "randomSeed": 42,
            "responseFormat": {
                "type": "json_schema",
                "jsonSchema": { "name": "result", "schemaDefinition": { "type": "object", "properties": { "maxTokens": { "type": "number" } } } },
            },
            "presencePenalty": 0.1,
            "frequencyPenalty": 0.2,
            "parallelToolCalls": true,
            "safePrompt": true,
            "messages": [
                { "role": "system", "content": "Be precise" },
                { "role": "user", "content": [
                    { "type": "text", "text": "describe" },
                    { "type": "image_url", "imageUrl": "data:image/png;base64,aGVsbG8=" },
                ]},
            ],
        });
        payload["messages"] = json!([
            { "role": "assistant", "prefix": false, "content": [{ "type": "text", "text": "hi" }], "toolCalls": [{ "id": "abc123456", "type": "function", "function": { "name": "lookup", "arguments": "{}" }, "index": 0 }] },
            { "role": "tool", "toolCallId": "abc123456", "name": "lookup", "content": [{ "type": "text", "text": "found" }] },
        ]);

        let wire = to_mistral_wire_payload(&payload);
        let obj = wire.as_object().unwrap();
        assert_eq!(obj["max_tokens"], 123);
        assert_eq!(obj["prompt_mode"], "reasoning");
        assert_eq!(obj["reasoning_effort"], "high");
        assert_eq!(
            obj["tool_choice"],
            json!({ "type": "function", "function": { "name": "lookup" } })
        );
        assert_eq!(obj["prompt_cache_key"], "session-1");
        assert_eq!(obj["top_p"], 0.9);
        assert_eq!(obj["random_seed"], 42);
        assert_eq!(obj["presence_penalty"], 0.1);
        assert_eq!(obj["frequency_penalty"], 0.2);
        assert_eq!(obj["parallel_tool_calls"], true);
        assert_eq!(obj["safe_prompt"], true);
        assert_eq!(
            obj["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": { "name": "result", "schema": { "type": "object", "properties": { "maxTokens": { "type": "number" } } } },
            })
        );
        assert!(!obj.contains_key("maxTokens"));
        assert!(!obj.contains_key("promptMode"));
        assert!(!obj.contains_key("promptCacheKey"));
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages[0]["tool_calls"][0]["id"], "abc123456");
        assert_eq!(messages[1]["tool_call_id"], "abc123456");
    }

    #[test]
    fn to_chat_messages_replays_assistant_and_tool_result() {
        // Port of mistral-http-transport.test.ts "serializes assistant thinking,
        // tool calls, and tool results for replay".
        let model = mistral_model("mistral-large-latest");
        let messages = vec![
            Message::Assistant({
                let mut a = AssistantMessage::new();
                a.set_api_provider_model("mistral-conversations", "mistral", &model.id);
                a.set_content(vec![
                    ContentBlock::thinking("reason"),
                    ContentBlock::text("answer"),
                    ContentBlock::tool_call("abc123456", "lookup", json!({ "query": "pi" })),
                ]);
                a.set_stop_reason(StopReason::ToolUse);
                a
            }),
            Message::ToolResult(ToolResultMessage::new(
                "abc123456",
                "lookup",
                vec![
                    ContentBlock::text("found"),
                    ContentBlock::image("aGVsbG8=", "image/png"),
                ],
                false,
            )),
        ];
        let out = to_chat_messages(&messages, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["prefix"], false);
        assert_eq!(
            out[0]["content"],
            json!([
                { "type": "thinking", "thinking": [{ "type": "text", "text": "reason" }] },
                { "type": "text", "text": "answer" },
            ])
        );
        assert_eq!(
            out[0]["toolCalls"],
            json!([{ "id": "abc123456", "type": "function", "function": { "name": "lookup", "arguments": "{\"query\":\"pi\"}" }, "index": 0 }])
        );
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["toolCallId"], "abc123456");
        assert_eq!(
            out[1]["content"],
            json!([
                { "type": "text", "text": "found" },
                { "type": "image_url", "imageUrl": "data:image/png;base64,aGVsbG8=" },
            ])
        );
    }

    #[test]
    fn to_function_tools_sets_strict() {
        let tools = vec![
            json_tool(
                "plain",
                "Plain tool",
                &json!({ "type": "object", "properties": {} }),
            ),
            crate::types::Tool {
                name: "strict".to_string(),
                description: "Strict tool".to_string(),
                parameters: json!({ "type": "object", "properties": { "value": { "type": "string" } } }),
                constrained_sampling: Some(crate::types::ConstrainedSampling::JsonSchema {
                    strict: crate::types::StrictPreference::Require,
                }),
            },
        ];
        let out = to_function_tools(&tools).unwrap();
        assert_eq!(out[0]["function"]["strict"], false);
        assert_eq!(out[1]["function"]["strict"], true);
        assert_eq!(
            out[1]["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(
            out[1]["function"]["parameters"]["required"],
            json!(["value"])
        );
        // Prefer-strict with a convertible schema resolves to true as well.
        let prefer = crate::types::Tool {
            name: "prefer".to_string(),
            description: "".to_string(),
            parameters: json!({ "type": "object", "properties": { "a": { "type": "string" }, "b": { "type": "string" } } }),
            constrained_sampling: Some(crate::types::ConstrainedSampling::JsonSchema {
                strict: crate::types::StrictPreference::Prefer,
            }),
        };
        let out = to_function_tools(&[prefer]).unwrap();
        assert_eq!(out[0]["function"]["strict"], true);
        // Non-required property is null-wrapped in an anyOf union.
        let params = &out[0]["function"]["parameters"];
        assert_eq!(
            params["properties"]["a"],
            json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })
        );
        assert_eq!(
            params["properties"]["b"],
            json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })
        );
        assert_eq!(params["required"], json!(["a", "b"]));
    }

    #[test]
    fn build_chat_payload_includes_system_prompt_and_options() {
        let model = mistral_model("mistral-large-latest");
        let context = Context {
            system_prompt: Some("Be precise".to_string()),
            messages: vec![Message::User(UserContent::blocks(
                vec![
                    ContentBlock::text("describe"),
                    ContentBlock::image("aGVsbG8=", "image/png"),
                ],
                1,
            ))],
            ..Default::default()
        };
        let options = MistralOptions {
            base: StreamOptions {
                temperature: Some(0.5),
                max_tokens: Some(123),
                session_id: Some("session-1".to_string()),
                ..Default::default()
            },
            tool_choice: Some(json!({ "type": "function", "function": { "name": "lookup" } })),
            prompt_mode: Some("reasoning".to_string()),
            reasoning_effort: Some("high".to_string()),
        };
        let payload = build_chat_payload(&model, &context, &context.messages, &options).unwrap();
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][0]["content"], "Be precise");
        assert_eq!(payload["messages"][1]["content"][1]["type"], "image_url");
        assert_eq!(
            payload["messages"][1]["content"][1]["imageUrl"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(payload["maxTokens"], 123);
        assert_eq!(payload["promptMode"], "reasoning");
        assert_eq!(payload["reasoningEffort"], "high");
        assert_eq!(payload["promptCacheKey"], "session-1");
        assert_eq!(
            payload["toolChoice"],
            json!({ "type": "function", "function": { "name": "lookup" } })
        );

        // cacheRetention none omits the prompt cache key.
        let options_none = MistralOptions {
            base: StreamOptions {
                session_id: Some("session-1".to_string()),
                cache_retention: Some("none".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let payload =
            build_chat_payload(&model, &context, &context.messages, &options_none).unwrap();
        assert!(!payload.as_object().unwrap().contains_key("promptCacheKey"));

        let empty_session = MistralOptions {
            base: StreamOptions {
                session_id: Some(String::new()),
                ..Default::default()
            },
            ..Default::default()
        };
        let payload =
            build_chat_payload(&model, &context, &context.messages, &empty_session).unwrap();
        assert!(!payload.as_object().unwrap().contains_key("promptCacheKey"));
    }

    // ------------------------------------------------------------------
    // Stream consumption
    // ------------------------------------------------------------------

    fn run_consume(
        events: &[SseEvent],
        model: &Model,
    ) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
        let mut output = new_output(model);
        let mut pushed = Vec::new();
        consume_chat_stream(events, &mut output, &mut |e| pushed.push(e), model).unwrap();
        (output, pushed)
    }

    #[test]
    fn consumes_thinking_text_tool_calls_and_cached_usage() {
        // Port of mistral-http-transport.test.ts "parses native thinking, text,
        // tool calls, and cached-token usage".
        let model = mistral_model("mistral-large-latest");
        let events = vec![
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":null,"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"reason"}]}]}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":null,"delta":{"content":[{"type":"text","text":"answer"}]}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":null,"delta":{"tool_calls":[{"id":"abc123456","index":0,"function":{"name":"lookup","arguments":"{\"query\":"}}]}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":"tool_calls","delta":{"tool_calls":[{"id":"abc123456","index":0,"function":{"name":"lookup","arguments":"\"pi\"}"}}]}}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            ),
        ];
        let (message, _) = run_consume(&events, &model);
        assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
        assert_eq!(message.raw_stop_reason().unwrap(), "tool_calls");
        assert_eq!(message.response_id().unwrap(), "response-1");
        assert_eq!(message.content().len(), 3);
        match &message.content()[0] {
            ContentBlock::Thinking { thinking, .. } => assert_eq!(thinking, "reason"),
            b => panic!("expected thinking: {b:?}"),
        }
        match &message.content()[1] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "answer"),
            b => panic!("expected text: {b:?}"),
        }
        match &message.content()[2] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "abc123456");
                assert_eq!(name, "lookup");
                assert_eq!(arguments["query"], "pi");
            }
            b => panic!("expected toolCall: {b:?}"),
        }
        let usage = message.usage().unwrap();
        assert_eq!(usage.input, 7);
        assert_eq!(usage.output, 4);
        assert_eq!(usage.cache_read, 3);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.total_tokens, 14);
    }

    #[test]
    fn stream_consumer_keeps_block_lifecycle_across_incremental_frames() {
        let model = mistral_model("mistral-large-latest");
        let events = [
            sse(
                r#"{"id":"response-1","choices":[{"index":0,"finish_reason":null,"delta":{"content":"first"}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","choices":[{"index":0,"finish_reason":"stop","delta":{"content":"second"}}]}"#,
            ),
        ];
        let mut output = new_output(&model);
        let mut state = MistralStreamState::default();
        let mut pushed = Vec::new();

        consume_chat_stream_into(
            &events[..1],
            &mut state,
            &mut output,
            &mut |event| pushed.push(event),
            &model,
        )
        .unwrap();
        assert!(pushed
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::TextStart { .. })));
        assert!(!pushed
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::TextEnd { .. })));

        consume_chat_stream_into(
            &events[1..],
            &mut state,
            &mut output,
            &mut |event| pushed.push(event),
            &model,
        )
        .unwrap();
        finish_mistral_stream(&mut state, &mut output, &mut |event| pushed.push(event));

        assert_eq!(
            output.content().iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }),
            Some("firstsecond")
        );
        assert_eq!(
            pushed
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::TextStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            pushed
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::TextEnd { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn tool_call_end_events_preserve_first_seen_order() {
        let model = mistral_model("mistral-large-latest");
        let events = [
            sse(
                r#"{"id":"response-1","choices":[{"index":0,"finish_reason":null,"delta":{"tool_calls":[{"id":"z12345678","index":0,"function":{"name":"late","arguments":"{}"}}]}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","choices":[{"index":0,"finish_reason":"tool_calls","delta":{"tool_calls":[{"id":"a12345678","index":1,"function":{"name":"early","arguments":"{}"}}]}}]}"#,
            ),
        ];
        let (_output, pushed) = run_consume(&events, &model);
        let ended: Vec<usize> = pushed
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ToolCallEnd { content_index, .. } => Some(*content_index),
                _ => None,
            })
            .collect();
        assert_eq!(ended, vec![0, 1]);
    }

    #[test]
    fn malformed_tool_arguments_use_the_provider_object_fallback() {
        let model = mistral_model("mistral-large-latest");
        let events = [sse(
            r#"{"id":"response-1","choices":[{"index":0,"finish_reason":"tool_calls","delta":{"tool_calls":[{"id":"abc123456","index":0,"function":{"name":"lookup","arguments":false}}]}}]}"#,
        )];
        let (output, pushed) = run_consume(&events, &model);
        assert!(matches!(
            output.content().first(),
            Some(ContentBlock::ToolCall { arguments, .. }) if arguments == &json!({})
        ));
        assert!(pushed.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::ToolCallDelta { delta, .. } if delta == "{}"
        )));
    }

    #[test]
    fn consumes_plain_string_content_as_text() {
        let model = mistral_model("mistral-large-latest");
        let events = vec![
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":null,"delta":{"content":"hel"}}]}"#,
            ),
            sse(
                r#"{"id":"response-1","model":"mistral-large-latest","choices":[{"index":0,"finish_reason":"stop","delta":{"content":"lo 🌍"}}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            ),
        ];
        let (message, pushed) = run_consume(&events, &model);
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        let text: String = message
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello 🌍");
        assert!(pushed
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextStart { .. })));
        assert!(pushed
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
        assert!(pushed
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextEnd { .. })));
    }

    #[test]
    fn raw_stop_reasons_match_upstream() {
        // Port of mistral-raw-stop-reason.test.ts.
        for (finish_reason, expected_stop, expected_error) in [
            ("stop", StopReason::Stop, None),
            (
                "error",
                StopReason::Error,
                Some("Provider stopped with: error".to_string()),
            ),
            (
                "unmapped_error",
                StopReason::Error,
                Some("Provider stopped with: unmapped_error".to_string()),
            ),
        ] {
            let event = json!({
                "id": "mistral-response-id",
                "model": "devstral-medium-latest",
                "choices": [{ "index": 0, "finish_reason": finish_reason, "delta": {} }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1 },
            });
            let model = mistral_model("devstral-medium-latest");
            let (message, _) = run_consume(&[sse(&event.to_string())], &model);
            assert_eq!(
                message.stop_reason(),
                Some(expected_stop),
                "{finish_reason}"
            );
            assert_eq!(message.raw_stop_reason().unwrap(), finish_reason);
            assert_eq!(
                message.error_message().map(|s| s.to_string()),
                expected_error,
                "{finish_reason}"
            );
        }
    }

    #[test]
    fn missing_finish_reason_is_pending_error() {
        let model = mistral_model("mistral-large-latest");
        let events = vec![sse(
            r#"{"id":"x","choices":[{"index":0,"finish_reason":null,"delta":{"content":"hi"}}]}"#,
        )];
        let mut output = new_output(&model);
        let mut pushed = Vec::new();
        consume_chat_stream(&events, &mut output, &mut |e| pushed.push(e), &model).unwrap();
        assert_eq!(output.stop_reason(), Some(StopReason::Pending));
        let text: String = output
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi");
    }

    // ------------------------------------------------------------------
    // Header / error surfaces
    // ------------------------------------------------------------------

    #[test]
    fn headers_defaults_and_affinity() {
        let mut model = mistral_model("mistral-large-latest");
        let base = StreamOptions {
            session_id: Some("session-1".to_string()),
            ..Default::default()
        };
        let headers = build_mistral_headers(&model, "secret", &base);
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(headers.get("x-affinity").unwrap(), "session-1");
        assert!(headers.contains_key("user-agent"));

        let base = StreamOptions {
            session_id: Some(String::new()),
            ..Default::default()
        };
        assert!(!build_mistral_headers(&model, "secret", &base).contains_key("x-affinity"));

        // cacheRetention none -> no x-affinity.
        let base = StreamOptions {
            session_id: Some("session-1".to_string()),
            cache_retention: Some("none".to_string()),
            ..Default::default()
        };
        let headers = build_mistral_headers(&model, "secret", &base);
        assert!(!headers.contains_key("x-affinity"));

        // Model static headers override case-insensitively.
        model.headers = Some(BTreeMap::from([
            ("Authorization".to_string(), "Bearer model-key".to_string()),
            ("X-Affinity".to_string(), "model-affinity".to_string()),
        ]));
        let base = StreamOptions {
            session_id: Some("automatic-affinity".to_string()),
            ..Default::default()
        };
        let headers = build_mistral_headers(&model, "secret", &base);
        assert_eq!(headers.get("authorization").unwrap(), "Bearer model-key");
        assert_eq!(headers.get("x-affinity").unwrap(), "model-affinity");

        // Request headers delete with null values; explicit affinity suppresses
        // the automatic one even when null (port of the "honors case-insensitive
        // header overrides" test).
        let mut request_headers = crate::types::ProviderHeaders::new();
        request_headers.insert("authorization".to_string(), None);
        request_headers.insert("x-affinity".to_string(), None);
        request_headers.insert("User-Agent".to_string(), Some("custom-agent".to_string()));
        let base = StreamOptions {
            session_id: Some("automatic-affinity".to_string()),
            base: crate::types::ProviderRequestOptions {
                headers: Some(request_headers.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let headers = build_mistral_headers(&model, "request-key", &base);
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-affinity"));
        assert_eq!(headers.get("user-agent").unwrap(), "custom-agent");

        // Without an explicit affinity key (model or request), the automatic
        // affinity applies.
        let mut plain_model = mistral_model("mistral-large-latest");
        plain_model.headers = None;
        let mut request_headers = crate::types::ProviderHeaders::new();
        request_headers.insert("User-Agent".to_string(), Some("custom-agent".to_string()));
        let base = StreamOptions {
            base: crate::types::ProviderRequestOptions {
                headers: Some(request_headers),
                ..Default::default()
            },
            session_id: Some("automatic-affinity".to_string()),
            ..Default::default()
        };
        let headers = build_mistral_headers(&plain_model, "request-key", &base);
        assert_eq!(headers.get("x-affinity").unwrap(), "automatic-affinity");
        // And a model-level affinity suppresses the automatic header too.
        let mut model_affinity = mistral_model("mistral-large-latest");
        model_affinity.headers = Some(BTreeMap::from([(
            "X-Affinity".to_string(),
            "model-affinity".to_string(),
        )]));
        let base = StreamOptions {
            session_id: Some("automatic-affinity".to_string()),
            ..Default::default()
        };
        let headers = build_mistral_headers(&model_affinity, "request-key", &base);
        assert_eq!(headers.get("x-affinity").unwrap(), "model-affinity");
    }

    #[test]
    fn formats_http_errors_with_truncated_bodies() {
        assert_eq!(
            format_mistral_error(
                "Forbidden",
                Some(403),
                Some("{\"message\":\"blocked by gateway\"}")
            ),
            "Mistral API error (403): {\"message\":\"blocked by gateway\"}"
        );
        assert_eq!(
            format_mistral_error("Forbidden", Some(403), None),
            "Mistral API error (403): Forbidden"
        );
        assert_eq!(format_mistral_error("boom", None, None), "boom");
        let long = "x".repeat(5000);
        let out = format_mistral_error("e", Some(500), Some(&long));
        assert!(out.contains("[truncated 1000 chars]"));
    }

    // ------------------------------------------------------------------
    // Tool call id normalization
    // ------------------------------------------------------------------

    #[test]
    fn mistral_tool_call_ids_are_stable_and_collision_safe() {
        let mut normalizer = MistralToolCallIdNormalizer::default();
        let a = normalizer.normalize("toolu_01A1");
        assert_eq!(a.chars().count(), MISTRAL_TOOL_CALL_ID_LENGTH);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(normalizer.normalize("toolu_01A1"), a);
        // A second id that hashes to the same short id must not collide.
        let b = normalizer.normalize("toolu_01B2");
        assert_ne!(a, b);
        assert_eq!(normalizer.normalize("toolu_01B2"), b);
    }

    // ------------------------------------------------------------------
    // Reasoning controls (port of mistral-reasoning-mode.test.ts)
    // ------------------------------------------------------------------

    fn simple_opts(
        reasoning: Option<crate::types::ThinkingLevel>,
        session_id: Option<&str>,
        cache_retention: Option<&str>,
    ) -> SimpleStreamOptions {
        SimpleStreamOptions {
            base: StreamOptions {
                session_id: session_id.map(|s| s.to_string()),
                cache_retention: cache_retention.map(|s| s.to_string()),
                ..Default::default()
            },
            reasoning,
            ..Default::default()
        }
    }

    #[test]
    fn reasoning_controls_select_effort_vs_prompt_mode() {
        let small = mistral_model("mistral-small-2603");
        let (prompt_mode, effort) = resolve_reasoning_controls(
            &small,
            &simple_opts(Some(crate::types::ThinkingLevel::Medium), None, None),
        );
        assert_eq!(effort.as_deref(), Some("high"));
        assert_eq!(prompt_mode, None);

        let (prompt_mode, effort) =
            resolve_reasoning_controls(&small, &simple_opts(None, None, None));
        assert_eq!(effort, None);
        assert_eq!(prompt_mode, None);

        let magistral = mistral_model("magistral-medium-latest");
        let (prompt_mode, effort) = resolve_reasoning_controls(
            &magistral,
            &simple_opts(Some(crate::types::ThinkingLevel::Medium), None, None),
        );
        assert_eq!(prompt_mode.as_deref(), Some("reasoning"));
        assert_eq!(effort, None);

        let medium35 = mistral_model("mistral-medium-3.5");
        let (prompt_mode, effort) = resolve_reasoning_controls(
            &medium35,
            &simple_opts(Some(crate::types::ThinkingLevel::Medium), None, None),
        );
        assert_eq!(effort.as_deref(), Some("high"));
        assert_eq!(prompt_mode, None);

        let (prompt_mode, effort) =
            resolve_reasoning_controls(&medium35, &simple_opts(None, None, None));
        assert_eq!(effort, None);
        assert_eq!(prompt_mode, None);
    }

    #[test]
    fn stream_simple_sets_prompt_cache_key_from_session() {
        let large = mistral_model("mistral-large-latest");
        let options = simple_opts(None, Some("session-123"), None);
        let (prompt_mode, effort) = resolve_reasoning_controls(&large, &options);
        assert_eq!(prompt_mode, None);
        assert_eq!(effort, None);
        // The session id feeds the payload through MistralOptions.base.session_id.
        let go = MistralOptions {
            base: options.base.clone(),
            tool_choice: None,
            prompt_mode,
            reasoning_effort: effort,
        };
        let payload = build_chat_payload(&large, &Context::default(), &[], &go).unwrap();
        assert_eq!(payload["promptCacheKey"], "session-123");

        let options = simple_opts(None, Some("session-123"), Some("none"));
        let go = MistralOptions {
            base: options.base.clone(),
            tool_choice: None,
            prompt_mode: None,
            reasoning_effort: None,
        };
        let payload = build_chat_payload(&large, &Context::default(), &[], &go).unwrap();
        assert!(!payload.as_object().unwrap().contains_key("promptCacheKey"));
    }

    #[test]
    fn cached_usage_uses_first_non_null_property_even_when_malformed() {
        let usage = json!({
            "promptTokensDetails": { "cachedTokens": "not-a-number" },
            "numCachedTokens": 4,
        });
        assert_eq!(get_mistral_cached_prompt_tokens(&usage, 10), 0);
    }

    // ------------------------------------------------------------------
    // SSE bytewise parsing through the port's parser
    // ------------------------------------------------------------------

    #[test]
    fn parses_bytewise_utf8_across_chunks() {
        // Port of "parses SSE and UTF-8 sequences split across transport chunks".
        let body = "data: {\"id\":\"response-bytewise\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{\"content\":\"héllo 🌍\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\r\n\r\ndata: [DONE]\r\n\r\n";
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for chunk in body.as_bytes().chunks(3) {
            for event in parser.push_bytes(chunk) {
                if event.data.trim() != "[DONE]" {
                    events.push(event);
                }
            }
        }
        let model = mistral_model("mistral-large-latest");
        let (message, _) = run_consume(&events, &model);
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        let text: String = message
            .content()
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "héllo 🌍");
    }

    #[test]
    fn stream_without_key_is_terminal_error() {
        let model = mistral_model("mistral-large-latest");
        let s = stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            None,
            &MistralOptions::default(),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, msg) = rt.block_on(s.collect());
        assert!(matches!(&events[0], AssistantMessageEvent::Error { .. }));
        let err = msg.error_message().unwrap_or("").to_string();
        assert!(err.contains("No API key for provider: mistral"), "{err}");
    }
}
