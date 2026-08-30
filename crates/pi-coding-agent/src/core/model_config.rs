//! Immutable, credential-blind models.json snapshot — port of
//! `packages/coding-agent/src/core/model-config.ts`.
//!
//! Loads `~/.pi/agent/models.json` (merged over the bundled model catalog by
//! the provider composer), stripping `//` comments and trailing commas before
//! JSON parsing, exactly like upstream. Schema validation errors are captured
//! as a config `error` string rather than thrown.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::settings::strip_bom;

/// Path to the user models.json (upstream `getModelsJsonPath`).
pub fn models_json_path() -> Option<std::path::PathBuf> {
    let agent_dir = crate::config::get_agent_dir();
    let path = agent_dir.join("models.json");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// models.json schema (typebox schemas from model-config.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelMapConfig {
    pub off: Option<ValueOrNull>,
    pub minimal: Option<ValueOrNull>,
    pub low: Option<ValueOrNull>,
    pub medium: Option<ValueOrNull>,
    pub high: Option<ValueOrNull>,
    pub xhigh: Option<ValueOrNull>,
    pub max: Option<ValueOrNull>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ValueOrNull {
    Str(String),
    Null,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostConfig {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTierConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTierConfig {
    pub input_tokens_above: f64,
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(rename = "cacheWrite", skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// A model definition under `providers.<id>.models` (upstream
/// `ModelDefinitionSchema`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMapConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

/// A model override under `providers.<id>.modelOverrides` (upstream
/// `ModelOverrideSchema`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModelOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMapConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostOverrideConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostOverrideConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTierOverrideConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTierOverrideConfig {
    pub input_tokens_above: f64,
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(rename = "cacheWrite", skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// A provider config under `providers.<id>` (upstream `ProviderConfigSchema`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonProvider {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelsJsonModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, ModelsJsonModelOverride>>,
}

// ---------------------------------------------------------------------------
// JSON comment stripping (upstream utils/json.ts `stripJsonComments`)
// ---------------------------------------------------------------------------

/// Strip `//` line comments and trailing commas, leaving string literals
/// untouched. Block comments (`/* */`) are not stripped by upstream and are
/// intentionally not handled here.
pub fn strip_json_comments(input: &str) -> String {
    // Pass 1: remove line comments, keeping strings.
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            out.push(c);
            let mut escaped = false;
            for next in chars.by_ref() {
                out.push(next);
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == '"' {
                    break;
                }
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            // Skip until end of line.
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    // Pass 2: remove trailing commas before } or ] (string-aware).
    // Mirrors upstream /"(?:\\.|[^"\\])*"|,(\s*[}\]])/g where a comma is
    // only removed when the next non-whitespace character closes an object or
    // array.
    let bytes: Vec<char> = out.chars().collect();
    let mut result = String::with_capacity(out.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let c = bytes[index];
        if c == '"' {
            // Copy the string verbatim.
            result.push(c);
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let next = bytes[index];
                result.push(next);
                index += 1;
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == '"' {
                    break;
                }
            }
            continue;
        }
        if c == ',' {
            // Look ahead past whitespace.
            let mut j = index + 1;
            while j < bytes.len() && bytes[j].is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == '}' || bytes[j] == ']') {
                index += 1; // drop the comma
                continue;
            }
        }
        result.push(c);
        index += 1;
    }
    result
}

type SchemaError = (String, String);

fn schema_error(
    errors: &mut Vec<SchemaError>,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    errors.push((path.into(), message.into()));
}

fn object_value<'a>(
    value: &'a Value,
    path: &str,
    errors: &mut Vec<SchemaError>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match value.as_object() {
        Some(object) => Some(object),
        None => {
            schema_error(errors, path, "Expected an object.");
            None
        }
    }
}

fn validate_string_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    required: bool,
    non_empty: bool,
    errors: &mut Vec<SchemaError>,
) {
    let Some(value) = object.get(key) else {
        if required {
            schema_error(
                errors,
                format!("{path}.{key}"),
                "Expected required property.",
            );
        }
        return;
    };
    let Some(string) = value.as_str() else {
        schema_error(errors, format!("{path}.{key}"), "Expected a string.");
        return;
    };
    if non_empty && string.is_empty() {
        schema_error(
            errors,
            format!("{path}.{key}"),
            "Expected string length greater than or equal to 1.",
        );
    }
}

fn validate_bool_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    errors: &mut Vec<SchemaError>,
) {
    if let Some(value) = object.get(key) {
        if !value.is_boolean() {
            schema_error(errors, format!("{path}.{key}"), "Expected a boolean.");
        }
    }
}

fn validate_number_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    required: bool,
    errors: &mut Vec<SchemaError>,
) {
    let Some(value) = object.get(key) else {
        if required {
            schema_error(
                errors,
                format!("{path}.{key}"),
                "Expected required property.",
            );
        }
        return;
    };
    if !value.is_number() {
        schema_error(errors, format!("{path}.{key}"), "Expected a number.");
    }
}

fn validate_string_record(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for (key, value) in object {
        if !value.is_string() {
            schema_error(errors, format!("{path}.{key}"), "Expected a string.");
        }
    }
}

fn validate_input(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(values) = value.as_array() else {
        schema_error(errors, path, "Expected an array.");
        return;
    };
    for (index, value) in values.iter().enumerate() {
        if !matches!(value.as_str(), Some("text" | "image")) {
            schema_error(
                errors,
                format!("{path}.{index}"),
                "Expected \"text\" or \"image\".",
            );
        }
    }
}

fn validate_thinking_level_map(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for (key, value) in object {
        if !value.is_string() && !value.is_null() {
            schema_error(
                errors,
                format!("{path}.{key}"),
                "Expected a string or null.",
            );
        }
    }
}

fn validate_cost(value: &Value, path: &str, override_cost: bool, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    validate_number_field(object, "input", path, !override_cost, errors);
    validate_number_field(object, "output", path, !override_cost, errors);
    validate_number_field(object, "cacheRead", path, !override_cost, errors);
    validate_number_field(object, "cacheWrite", path, !override_cost, errors);
    let Some(tiers) = object.get("tiers") else {
        return;
    };
    let Some(tiers) = tiers.as_array() else {
        schema_error(errors, format!("{path}.tiers"), "Expected an array.");
        return;
    };
    for (index, tier) in tiers.iter().enumerate() {
        let tier_path = format!("{path}.tiers.{index}");
        let Some(tier) = object_value(tier, &tier_path, errors) else {
            continue;
        };
        validate_number_field(tier, "inputTokensAbove", &tier_path, true, errors);
        validate_number_field(tier, "input", &tier_path, true, errors);
        validate_number_field(tier, "output", &tier_path, true, errors);
        validate_number_field(tier, "cacheRead", &tier_path, true, errors);
        validate_number_field(tier, "cacheWrite", &tier_path, true, errors);
    }
}

fn validate_percentile_object(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for field in ["p50", "p75", "p90", "p99"] {
        validate_number_field(object, field, path, false, errors);
    }
}

fn validate_string_array(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(values) = value.as_array() else {
        schema_error(errors, path, "Expected an array.");
        return;
    };
    for (index, value) in values.iter().enumerate() {
        if !value.is_string() {
            schema_error(errors, format!("{path}.{index}"), "Expected a string.");
        }
    }
}

fn validate_open_router_routing(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for field in [
        "allow_fallbacks",
        "require_parameters",
        "zdr",
        "enforce_distillable_text",
    ] {
        validate_bool_field(object, field, path, errors);
    }
    if let Some(value) = object.get("data_collection") {
        if !matches!(value.as_str(), Some("deny" | "allow")) {
            schema_error(
                errors,
                format!("{path}.data_collection"),
                "Expected \"deny\" or \"allow\".",
            );
        }
    }
    for field in ["order", "only", "ignore", "quantizations"] {
        if let Some(value) = object.get(field) {
            validate_string_array(value, &format!("{path}.{field}"), errors);
        }
    }
    if let Some(value) = object.get("sort") {
        if value.is_string() {
            // The schema accepts a string sort mode without further fields.
        } else if let Some(sort) = object_value(value, &format!("{path}.sort"), errors) {
            validate_string_field(sort, "by", &format!("{path}.sort"), false, false, errors);
            if let Some(partition) = sort.get("partition") {
                if !partition.is_string() && !partition.is_null() {
                    schema_error(
                        errors,
                        format!("{path}.sort.partition"),
                        "Expected a string or null.",
                    );
                }
            }
        } else {
            schema_error(
                errors,
                format!("{path}.sort"),
                "Expected a string or object.",
            );
        }
    }
    if let Some(value) = object.get("max_price") {
        if let Some(price) = object_value(value, &format!("{path}.max_price"), errors) {
            for field in ["prompt", "completion", "image", "audio", "request"] {
                if let Some(value) = price.get(field) {
                    if !value.is_number() && !value.is_string() {
                        schema_error(
                            errors,
                            format!("{path}.max_price.{field}"),
                            "Expected a number or string.",
                        );
                    }
                }
            }
        }
    }
    for field in ["preferred_min_throughput", "preferred_max_latency"] {
        let Some(value) = object.get(field) else {
            continue;
        };
        if !value.is_number() {
            validate_percentile_object(value, &format!("{path}.{field}"), errors);
        }
    }
}

fn validate_chat_template_record(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for (key, value) in object {
        if value.is_string() || value.is_number() || value.is_boolean() || value.is_null() {
            continue;
        }
        let Some(variable) = object_value(value, &format!("{path}.{key}"), errors) else {
            continue;
        };
        match variable.get("$var").and_then(Value::as_str) {
            Some("thinking.enabled" | "thinking.effort") => {}
            _ => schema_error(
                errors,
                format!("{path}.{key}.$var"),
                "Expected a supported thinking variable.",
            ),
        }
        validate_bool_field(variable, "omitWhenOff", &format!("{path}.{key}"), errors);
    }
}

fn validate_compat(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for field in [
        "supportsStore",
        "supportsDeveloperRole",
        "supportsReasoningEffort",
        "supportsUsageInStreaming",
        "requiresToolResultName",
        "requiresAssistantAfterToolResult",
        "requiresThinkingAsText",
        "requiresReasoningContentOnAssistantMessages",
        "supportsOpenAIGrammarTools",
        "supportsStrictMode",
        "sendSessionAffinityHeaders",
        "supportsLongCacheRetention",
        "supportsAdditionalTools",
        "supportsToolSearch",
        "supportsEagerToolInputStreaming",
        "supportsCacheControlOnTools",
        "supportsTemperature",
        "forceAdaptiveThinking",
        "allowEmptySignature",
        "supportsStrictTools",
        "supportsToolReferences",
    ] {
        validate_bool_field(object, field, path, errors);
    }
    if let Some(value) = object.get("maxTokensField") {
        if !matches!(value.as_str(), Some("max_completion_tokens" | "max_tokens")) {
            schema_error(
                errors,
                format!("{path}.maxTokensField"),
                "Expected a supported token field.",
            );
        }
    }
    if let Some(value) = object.get("thinkingFormat") {
        if !matches!(
            value.as_str(),
            Some(
                "openai"
                    | "openrouter"
                    | "together"
                    | "baseten"
                    | "deepseek"
                    | "zai"
                    | "qwen"
                    | "chat-template"
                    | "qwen-chat-template"
                    | "string-thinking"
                    | "ant-ling"
            )
        ) {
            schema_error(
                errors,
                format!("{path}.thinkingFormat"),
                "Expected a supported thinking format.",
            );
        }
    }
    for field in ["chatTemplateKwargs", "chatTemplateArgs"] {
        if let Some(value) = object.get(field) {
            validate_chat_template_record(value, &format!("{path}.{field}"), errors);
        }
    }
    if let Some(value) = object.get("cacheControlFormat") {
        if value.as_str() != Some("anthropic") {
            schema_error(
                errors,
                format!("{path}.cacheControlFormat"),
                "Expected \"anthropic\".",
            );
        }
    }
    if let Some(value) = object.get("openRouterRouting") {
        validate_open_router_routing(value, &format!("{path}.openRouterRouting"), errors);
    }
    if let Some(value) = object.get("vercelGatewayRouting") {
        let routing_path = format!("{path}.vercelGatewayRouting");
        if let Some(routing) = object_value(value, &routing_path, errors) {
            for field in ["only", "order"] {
                if let Some(value) = routing.get(field) {
                    validate_string_array(value, &format!("{routing_path}.{field}"), errors);
                }
            }
        }
    }
    if let Some(value) = object.get("deferredToolsMode") {
        if value.as_str() != Some("kimi") {
            schema_error(
                errors,
                format!("{path}.deferredToolsMode"),
                "Expected \"kimi\".",
            );
        }
    }
    if let Some(value) = object.get("sessionAffinityFormat") {
        if !matches!(
            value.as_str(),
            Some("openai" | "openai-nosession" | "openrouter")
        ) {
            schema_error(
                errors,
                format!("{path}.sessionAffinityFormat"),
                "Expected a supported session affinity format.",
            );
        }
    }
}

fn validate_model(value: &Value, path: &str, override_model: bool, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    if !override_model {
        validate_string_field(object, "id", path, true, true, errors);
    }
    for field in ["name", "api", "baseUrl"] {
        if !override_model || field == "name" {
            validate_string_field(object, field, path, false, true, errors);
        }
    }
    validate_bool_field(object, "reasoning", path, errors);
    if let Some(value) = object.get("thinkingLevelMap") {
        validate_thinking_level_map(value, &format!("{path}.thinkingLevelMap"), errors);
    }
    if let Some(value) = object.get("input") {
        validate_input(value, &format!("{path}.input"), errors);
    }
    if let Some(value) = object.get("cost") {
        validate_cost(value, &format!("{path}.cost"), override_model, errors);
    }
    for field in ["contextWindow", "maxTokens"] {
        validate_number_field(object, field, path, false, errors);
    }
    if let Some(value) = object.get("samplingParams") {
        if !value.is_object() {
            schema_error(
                errors,
                format!("{path}.samplingParams"),
                "Expected an object.",
            );
        }
    }
    if let Some(value) = object.get("headers") {
        validate_string_record(value, &format!("{path}.headers"), errors);
    }
    if let Some(value) = object.get("compat") {
        validate_compat(value, &format!("{path}.compat"), errors);
    }
}

fn validate_provider(value: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(object) = object_value(value, path, errors) else {
        return;
    };
    for field in ["name", "baseUrl", "apiKey", "api"] {
        validate_string_field(object, field, path, false, true, errors);
    }
    if let Some(value) = object.get("oauth") {
        if value.as_str() != Some("radius") {
            schema_error(errors, format!("{path}.oauth"), "Expected \"radius\".");
        }
    }
    if let Some(value) = object.get("headers") {
        validate_string_record(value, &format!("{path}.headers"), errors);
    }
    if let Some(value) = object.get("compat") {
        validate_compat(value, &format!("{path}.compat"), errors);
    }
    validate_bool_field(object, "authHeader", path, errors);
    if let Some(value) = object.get("models") {
        let Some(models) = value.as_array() else {
            schema_error(errors, format!("{path}.models"), "Expected an array.");
            return;
        };
        for (index, model) in models.iter().enumerate() {
            validate_model(model, &format!("{path}.models.{index}"), false, errors);
        }
    }
    if let Some(value) = object.get("modelOverrides") {
        let Some(overrides) = object_value(value, &format!("{path}.modelOverrides"), errors) else {
            return;
        };
        for (model_id, override_model) in overrides {
            validate_model(
                override_model,
                &format!("{path}.modelOverrides.{model_id}"),
                true,
                errors,
            );
        }
    }
}

fn validate_models_value(value: &Value) -> Vec<SchemaError> {
    let mut errors = Vec::new();
    let Some(root) = object_value(value, "root", &mut errors) else {
        return errors;
    };
    let Some(providers) = root.get("providers") else {
        schema_error(
            &mut errors,
            "providers",
            "Expected required property \"providers\".",
        );
        return errors;
    };
    let Some(providers) = object_value(providers, "providers", &mut errors) else {
        return errors;
    };
    for (provider_id, provider) in providers {
        validate_provider(provider, &format!("providers.{provider_id}"), &mut errors);
    }
    errors
}

// ---------------------------------------------------------------------------
// ModelConfig
// ---------------------------------------------------------------------------

/// One immutable load of models.json (upstream `ModelConfig`).
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    providers: BTreeMap<String, ModelsJsonProvider>,
    /// Preserve JSON insertion order for the upstream `Map.keys()` surface;
    /// the BTreeMap remains the keyed lookup/index.
    provider_order: Vec<String>,
    error: Option<String>,
}

impl ModelConfig {
    /// Load models.json from a path. A missing file yields an empty config
    /// with no error (upstream behavior). Structural/schema errors are
    /// captured in `error()`.
    pub fn load(models_json_path: Option<&Path>) -> ModelConfig {
        let Some(models_json_path) = models_json_path else {
            return ModelConfig {
                providers: BTreeMap::new(),
                provider_order: Vec::new(),
                error: None,
            };
        };
        let path = crate::core::settings::resolve_path(&models_json_path.to_string_lossy());
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    provider_order: Vec::new(),
                    error: None,
                }
            }
            Err(e) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    provider_order: Vec::new(),
                    error: Some(format!(
                        "Failed to load models.json: {e}\n\nFile: {}",
                        path.display()
                    )),
                }
            }
        };

        let stripped = strip_json_comments(strip_bom(&content));
        let parsed: Value = match serde_json::from_str(&stripped) {
            Ok(value) => value,
            Err(e) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    provider_order: Vec::new(),
                    error: Some(format!(
                        "Failed to parse models.json: {e}\n\nFile: {}",
                        path.display()
                    )),
                }
            }
        };

        match Self::from_value(parsed) {
            Ok(config) => config,
            Err(errors) => {
                let details = errors
                    .iter()
                    .map(|(field, message)| format!("  - {field}: {message}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                ModelConfig {
                    providers: BTreeMap::new(),
                    provider_order: Vec::new(),
                    error: Some(format!(
                        "Invalid models.json schema:\n{details}\n\nFile: {}",
                        path.display()
                    )),
                }
            }
        }
    }

    /// Validate and build a ModelConfig from a parsed JSON value.
    /// Produces a list of `(jsonPath, message)` validation errors.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn from_value(value: Value) -> Result<ModelConfig, Vec<(String, String)>> {
        let errors = validate_models_value(&value);
        if !errors.is_empty() {
            return Err(errors);
        }
        let providers_value = value
            .as_object()
            .and_then(|object| object.get("providers"))
            .expect("models config validation checked providers");
        // Full typed parse. A failure here means a field-level schema error;
        // report the first offending path with serde's message.
        match serde_json::from_value::<BTreeMap<String, ModelsJsonProvider>>(
            providers_value.clone(),
        ) {
            Ok(map) => {
                let provider_order = providers_value
                    .as_object()
                    .expect("models config validation checked providers")
                    .keys()
                    .cloned()
                    .collect();
                Ok(ModelConfig {
                    providers: map,
                    provider_order,
                    error: None,
                })
            }
            Err(e) => {
                let message = e.to_string();
                // serde_json errors carry a path like `providers.demo.models[0].id`.
                Err(vec![(message.clone(), "Invalid value.".to_string())])
            }
        }
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<&ModelsJsonProvider> {
        self.providers.get(provider_id)
    }

    pub fn get_provider_ids(&self) -> impl Iterator<Item = &str> {
        self.provider_order.iter().map(|s| s.as_str())
    }

    pub fn get_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Directory typically used as `models.json`'s sibling for the file
    /// models store (upstream joins dirname(modelsPath)).
    pub fn models_store_path_for(models_path: &Path) -> PathBuf {
        models_path.with_file_name("models-store.json")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pi-model-config-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn missing_file_is_empty_config_without_error() {
        let cfg = ModelConfig::load(Some(Path::new("/nonexistent/models.json")));
        assert!(cfg.get_error().is_none());
        assert_eq!(cfg.get_provider_ids().count(), 0);
    }

    #[test]
    fn no_path_is_empty_config() {
        let cfg = ModelConfig::load(None);
        assert!(cfg.get_error().is_none());
    }

    #[test]
    fn provider_ids_preserve_models_json_insertion_order() {
        let cfg = ModelConfig::from_value(serde_json::json!({
            "providers": {
                "zeta": { "baseUrl": "https://zeta.example.com", "api": "openai-responses" },
                "alpha": { "baseUrl": "https://alpha.example.com", "api": "openai-responses" }
            }
        }))
        .expect("valid models.json");
        assert_eq!(
            cfg.get_provider_ids().collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn loads_providers_with_models() {
        let path = write_tmp(
            "valid",
            r#"{
  "providers": {
    "demo": {
      "name": "Demo",
      "baseUrl": "https://demo.example.com/v1",
      "api": "openai-responses",
      "apiKey": "$DEMO_KEY",
      "models": [
        { "id": "demo-1", "name": "Demo 1", "reasoning": false, "input": ["text", "image"],
          "cost": { "input": 0.5, "output": 1.5, "cacheRead": 0.1, "cacheWrite": 1.0 },
          "contextWindow": 200000, "maxTokens": 16384 }
      ]
    }
  }
}"#,
        );
        let cfg = ModelConfig::load(Some(&path));
        assert!(
            cfg.get_error().is_none(),
            "unexpected error: {:?}",
            cfg.get_error()
        );
        let provider = cfg.get_provider("demo").unwrap();
        assert_eq!(provider.name.as_deref(), Some("Demo"));
        let model = &provider.models.as_ref().unwrap()[0];
        assert_eq!(model.id, "demo-1");
        assert_eq!(model.cost.as_ref().unwrap().input, 0.5);
    }

    #[test]
    fn unknown_models_json_fields_are_accepted_and_ignored() {
        let config = ModelConfig::from_value(serde_json::json!({
            "x-parity-root-unknown": "ignored",
            "providers": {
                "demo": {
                    "baseUrl": "https://demo.example.com/v1",
                    "api": "openai-responses",
                    "x-parity-provider-unknown": {"nested": true},
                    "models": [{
                        "id": "demo-1",
                        "x-parity-model-unknown": [1, 2, 3]
                    }]
                }
            }
        }))
        .expect("Type.Object schemas allow additional properties");

        let provider = config.get_provider("demo").expect("demo provider");
        assert_eq!(provider.api.as_deref(), Some("openai-responses"));
        assert_eq!(provider.models.as_ref().expect("models")[0].id, "demo-1");
    }

    #[test]
    fn cost_tiers_require_both_cache_rates() {
        let base_error = ModelConfig::from_value(serde_json::json!({
            "providers": {
                "demo": {
                    "models": [{
                        "id": "demo-1",
                        "cost": {
                            "input": 1.0,
                            "output": 2.0,
                            "cacheRead": 3.0,
                            "cacheWrite": 4.0,
                            "tiers": [{
                                "inputTokensAbove": 1000,
                                "input": 5.0,
                                "output": 6.0
                            }]
                        }
                    }]
                }
            }
        }))
        .expect_err("model cost tiers require cache rates");
        assert!(base_error
            .iter()
            .any(|(path, _)| path.ends_with("cacheRead")));
        assert!(base_error
            .iter()
            .any(|(path, _)| path.ends_with("cacheWrite")));

        let override_error = ModelConfig::from_value(serde_json::json!({
            "providers": {
                "demo": {
                    "modelOverrides": {
                        "demo-1": {
                            "cost": {
                                "tiers": [{
                                    "inputTokensAbove": 1000,
                                    "input": 5.0,
                                    "output": 6.0,
                                    "cacheRead": 7.0
                                }]
                            }
                        }
                    }
                }
            }
        }))
        .expect_err("override cost tiers require cacheWrite");
        assert!(override_error
            .iter()
            .any(|(path, _)| path.ends_with("cacheWrite")));
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let path = write_tmp(
            "comments",
            r#"{
  // pi models overlay
  "providers": {
    "demo": {
      "baseUrl": "https://demo.example.com/v1",
      "api": "openai-responses",
      "models": [
        { "id": "demo-1", "reasoning": true, },
      ],
    },
  },
}"#,
        );
        let cfg = ModelConfig::load(Some(&path));
        assert!(
            cfg.get_error().is_none(),
            "unexpected error: {:?}",
            cfg.get_error()
        );
        assert!(cfg.get_provider("demo").is_some());
    }

    #[test]
    fn comment_lookalikes_inside_strings_survive() {
        let source = r#"{ "providers": { "demo": { "apiKey": "http://x/not-a-comment", "baseUrl": "https://y" } } }"#;
        let cfg =
            ModelConfig::from_value(serde_json::from_str(&strip_json_comments(source)).unwrap());
        assert!(cfg.is_ok());
    }

    #[test]
    fn parse_error_captured() {
        let path = write_tmp("badjson", "{ not json !!!");
        let cfg = ModelConfig::load(Some(&path));
        let err = cfg.get_error().unwrap();
        assert!(err.contains("Failed to parse models.json"), "{err}");
        assert!(err.contains("models.json"), "{err}");
    }

    #[test]
    fn schema_error_captured() {
        let path = write_tmp(
            "schemas",
            r#"{ "providers": { "demo": { "models": "not-an-array" } } }"#,
        );
        let cfg = ModelConfig::load(Some(&path));
        let err = cfg.get_error().unwrap();
        assert!(err.contains("Invalid models.json schema"), "{err}");
    }

    #[test]
    fn strips_bom_before_parse() {
        let path = write_tmp(
            "bom",
            format!("\u{feff}{}", r#"{ "providers": {} }"#).as_str(),
        );
        let cfg = ModelConfig::load(Some(&path));
        assert!(cfg.get_error().is_none());
    }

    #[test]
    fn empty_providers_allowed() {
        let path = write_tmp("empty", r#"{ "providers": {} }"#);
        let cfg = ModelConfig::load(Some(&path));
        assert!(cfg.get_error().is_none());
        assert_eq!(cfg.get_provider_ids().count(), 0);
    }

    #[test]
    fn trailing_comma_strip_works_outside_strings() {
        assert_eq!(strip_json_comments("{\"a\": 1,}"), "{\"a\": 1}");
        assert_eq!(strip_json_comments("{\"a\": [1, 2,],}"), "{\"a\": [1, 2]}");
        assert_eq!(strip_json_comments("// hi\n{\"a\": 1}"), "\n{\"a\": 1}");
        assert_eq!(
            strip_json_comments("{\"s\": \"a,b,\"}"),
            "{\"s\": \"a,b,\"}"
        );
        assert_eq!(
            strip_json_comments("{\"s\": \"// not comment\", \"a\": 1}"),
            "{\"s\": \"// not comment\", \"a\": 1}"
        );
    }
}
