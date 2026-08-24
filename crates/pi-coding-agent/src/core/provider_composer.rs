//! Provider/model composition — port of
//! `packages/coding-agent/src/core/provider-composer.ts`.
//!
//! This module composes the built-in provider catalog, the on-disk
//! `models.json` overlay, and (in the TypeScript port) extension provider
//! registrations into the final provider/model surface. The Rust port keeps
//! the credential-blind, pure-data composition functions (models overlay,
//! model overrides, compat merging, auth-status classification); the live
//! provider auth plumbing (apiKey/oauth auth objects) stays with the pi-ai
//! Models facade.

use std::collections::BTreeMap;

use pi_ai::model::Model;
use pi_ai::models::Provider;
use pi_ai::types::ProviderHeaders;
use serde_json::Value;

use crate::core::model_config::{ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider};

/// Apply a composed model catalog to a provider without rebuilding its
/// dispatch surface. In particular, deferred fetch/cancel hooks belong to the
/// provider's API implementation, not to an individual models.json entry, so
/// catalog overlays must retain them verbatim.
pub fn with_composed_models(provider: &Provider, models: Vec<Model>) -> Provider {
    let mut composed = provider.clone();
    composed.models = models;
    composed
}

/// Shallow-merge two JSON compat objects, deep-merging the nested routing and
/// template maps (upstream `mergeCompat`).
pub fn merge_compat(base: Option<&Value>, override_compat: Option<&Value>) -> Option<Value> {
    match (base, override_compat) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(override_compat)) => Some(override_compat.clone()),
        (Some(base), Some(override_compat)) => {
            let mut merged = base.clone();
            if let (Some(base_obj), Some(override_obj)) =
                (merged.as_object_mut(), override_compat.as_object())
            {
                for (key, override_value) in override_obj {
                    const NESTED_KEYS: &[&str] = &[
                        "openRouterRouting",
                        "vercelGatewayRouting",
                        "chatTemplateKwargs",
                        "chatTemplateArgs",
                    ];
                    if NESTED_KEYS.contains(&key.as_str()) {
                        let base_value = base_obj.get(key);
                        if matches!(base_value, Some(Value::Object(_)))
                            && override_value.is_object()
                        {
                            let mut nested = base_value.cloned().unwrap_or(Value::Null);
                            if let (Some(nested_obj), Some(override_nested)) =
                                (nested.as_object_mut(), override_value.as_object())
                            {
                                for (k, v) in override_nested {
                                    nested_obj.insert(k.clone(), v.clone());
                                }
                            }
                            base_obj.insert(key.clone(), nested);
                        } else {
                            base_obj.insert(key.clone(), override_value.clone());
                        }
                    } else {
                        base_obj.insert(key.clone(), override_value.clone());
                    }
                }
            }
            Some(merged)
        }
    }
}

/// Apply a model override to a catalog model (upstream `applyModelOverride`).
pub fn apply_model_override(model: &Model, override_config: &ModelsJsonModelOverride) -> Model {
    let mut result = model.clone();
    if let Some(name) = &override_config.name {
        result.name = name.clone();
    }
    if let Some(reasoning) = override_config.reasoning {
        result.reasoning = reasoning;
    }
    if let Some(map) = &override_config.thinking_level_map {
        result.thinking_level_map = Some(thinking_level_map_from_config(map));
    }
    if let Some(input) = &override_config.input {
        result.input = input
            .iter()
            .filter_map(|s| match s.as_str() {
                "text" => Some(pi_ai::model::ModelInput::Text),
                "image" => Some(pi_ai::model::ModelInput::Image),
                _ => None,
            })
            .collect();
    }
    if let Some(cost) = &override_config.cost {
        result.cost.input = cost.input.unwrap_or(result.cost.input);
        result.cost.output = cost.output.unwrap_or(result.cost.output);
        result.cost.cache_read = cost.cache_read.unwrap_or(result.cost.cache_read);
        result.cost.cache_write = cost.cache_write.unwrap_or(result.cost.cache_write);
        if let Some(tiers) = &cost.tiers {
            result.cost.tiers = Some(
                tiers
                    .iter()
                    .map(|t| pi_ai::model::ModelCostTier {
                        input_tokens_above: t.input_tokens_above as u64,
                        input: t.input,
                        output: t.output,
                        cache_read: t.cache_read.unwrap_or(0.0),
                        cache_write: t.cache_write.unwrap_or(0.0),
                    })
                    .collect(),
            );
        }
    }
    if let Some(window) = override_config.context_window {
        result.context_window = window as u64;
    }
    if let Some(max_tokens) = override_config.max_tokens {
        result.max_tokens = max_tokens as u64;
    }
    if let Some(params) = &override_config.sampling_params {
        result.sampling_params = Some(merge_json_object(result.sampling_params.take(), params));
    }
    if let Some(headers) = &override_config.headers {
        let mut merged = result.headers.take().unwrap_or_default();
        for (k, v) in headers {
            merged.insert(k.clone(), v.clone());
        }
        result.headers = if merged.is_empty() {
            None
        } else {
            Some(merged)
        };
    }
    if let Some(compat) = &override_config.compat {
        result.compat = merge_compat(result.compat.as_ref(), Some(compat));
    }
    result
}

/// Convert the config-level thinking-level map into the pi-ai
/// `ThinkingLevelMap` shape (`BTreeMap<ModelThinkingLevel, Option<String>>`).
fn thinking_level_map_from_config(
    map: &crate::core::model_config::ThinkingLevelMapConfig,
) -> BTreeMap<pi_ai::types::ModelThinkingLevel, Option<String>> {
    use crate::core::model_config::ValueOrNull;
    use pi_ai::types::ModelThinkingLevel as L;
    let mut out: BTreeMap<pi_ai::types::ModelThinkingLevel, Option<String>> = BTreeMap::new();
    let mut push = |key: L, value: &Option<ValueOrNull>| {
        if let Some(value) = value {
            out.insert(
                key,
                match value {
                    ValueOrNull::Str(s) => Some(s.clone()),
                    ValueOrNull::Null => None,
                },
            );
        }
    };
    push(L::Off, &map.off);
    push(L::Minimal, &map.minimal);
    push(L::Low, &map.low);
    push(L::Medium, &map.medium);
    push(L::High, &map.high);
    push(L::Xhigh, &map.xhigh);
    push(L::Max, &map.max);
    out
}

fn merge_json_object(base: Option<Value>, override_value: &Value) -> Value {
    match (base, override_value) {
        (Some(Value::Object(mut base_obj)), Value::Object(override_obj)) => {
            for (k, v) in override_obj {
                base_obj.insert(k.clone(), v.clone());
            }
            Value::Object(base_obj)
        }
        (_, other) => other.clone(),
    }
}

/// Build a `Model` from a models.json model definition (upstream
/// `modelFromJson`).
pub fn model_from_json(
    provider_id: &str,
    definition: &ModelsJsonModel,
    provider_config: &ModelsJsonProvider,
    defaults: Option<&Model>,
) -> Result<Model, String> {
    let api = definition
        .api
        .clone()
        .or_else(|| provider_config.api.clone())
        .or_else(|| defaults.map(|d| d.api.clone()))
        .ok_or_else(|| {
            format!(
                "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
                definition.id
            )
        })?;
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| provider_config.base_url.clone())
        .or_else(|| defaults.map(|d| d.base_url.clone()))
        .ok_or_else(|| {
            format!("Provider {provider_id}: \"baseUrl\" is required when defining custom models.")
        })?;
    if let Some(window) = definition.context_window {
        if window <= 0.0 {
            return Err(format!(
                "Provider {provider_id}, model {}: invalid contextWindow",
                definition.id
            ));
        }
    }
    if let Some(max_tokens) = definition.max_tokens {
        if max_tokens <= 0.0 {
            return Err(format!(
                "Provider {provider_id}, model {}: invalid maxTokens",
                definition.id
            ));
        }
    }
    let mut model = Model::new(
        definition.id.clone(),
        definition
            .name
            .clone()
            .unwrap_or_else(|| definition.id.clone()),
        api,
        provider_id,
    );
    model.base_url = base_url;
    model.reasoning = definition.reasoning.unwrap_or(false);
    if let Some(map) = &definition.thinking_level_map {
        model.thinking_level_map = Some(thinking_level_map_from_config(map));
    }
    model.input = match &definition.input {
        Some(input) => input
            .iter()
            .filter_map(|s| match s.as_str() {
                "text" => Some(pi_ai::model::ModelInput::Text),
                "image" => Some(pi_ai::model::ModelInput::Image),
                _ => None,
            })
            .collect(),
        None => vec![pi_ai::model::ModelInput::Text],
    };
    if let Some(cost) = &definition.cost {
        model.cost.input = cost.input;
        model.cost.output = cost.output;
        model.cost.cache_read = cost.cache_read;
        model.cost.cache_write = cost.cache_write;
        if let Some(tiers) = &cost.tiers {
            model.cost.tiers = Some(
                tiers
                    .iter()
                    .map(|t| pi_ai::model::ModelCostTier {
                        input_tokens_above: t.input_tokens_above as u64,
                        input: t.input,
                        output: t.output,
                        cache_read: t.cache_read.unwrap_or(0.0),
                        cache_write: t.cache_write.unwrap_or(0.0),
                    })
                    .collect(),
            );
        }
    }
    model.context_window = definition.context_window.unwrap_or(128_000.0) as u64;
    model.max_tokens = definition.max_tokens.unwrap_or(16_384.0) as u64;
    model.sampling_params = definition.sampling_params.clone();
    model.headers = definition.headers.clone();
    model.compat = merge_compat(provider_config.compat.as_ref(), definition.compat.as_ref());
    Ok(model)
}

/// Apply extension-provider models over the composed catalog (upstream
/// `applyExtension`). When `config.models` is absent the base models are
/// returned (baseUrl override when present); otherwise the extension's model
/// definitions replace/extend the layered catalog.
pub fn apply_extension(
    provider_id: &str,
    models: &[Model],
    config: &ProviderExtensionConfig,
) -> Result<Vec<Model>, String> {
    let Some(extension_models) = &config.models else {
        return Ok(match &config.base_url {
            Some(base_url) => models
                .iter()
                .map(|m| {
                    let mut model = m.clone();
                    model.base_url = base_url.clone();
                    model
                })
                .collect(),
            None => models.to_vec(),
        });
    };
    let mut result = Vec::new();
    for definition in extension_models {
        let defaults = models
            .iter()
            .find(|m| m.id == definition.id)
            .or_else(|| models.first());
        let api = definition
            .api
            .clone()
            .or_else(|| config.api.clone())
            .or_else(|| defaults.map(|d| d.api.clone()))
            .ok_or_else(|| {
                format!(
                    "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
                    definition.id
                )
            })?;
        let base_url = definition
            .base_url
            .clone()
            .or_else(|| config.base_url.clone())
            .or_else(|| defaults.map(|d| d.base_url.clone()))
            .ok_or_else(|| {
                format!(
                    "Provider {provider_id}: \"baseUrl\" is required when defining custom models."
                )
            })?;
        let mut model = Model::new(
            definition.id.clone(),
            definition
                .name
                .clone()
                .unwrap_or_else(|| definition.id.clone()),
            api,
            provider_id.to_string(),
        );
        model.base_url = base_url;
        model.reasoning = definition.reasoning.unwrap_or(false);
        if let Some(map) = &definition.thinking_level_map {
            model.thinking_level_map = Some(thinking_level_map_from_config(map));
        }
        model.input = match &definition.input {
            Some(input) => input
                .iter()
                .filter_map(|s| match s.as_str() {
                    "text" => Some(pi_ai::model::ModelInput::Text),
                    "image" => Some(pi_ai::model::ModelInput::Image),
                    _ => None,
                })
                .collect(),
            None => vec![pi_ai::model::ModelInput::Text],
        };
        if let Some(cost) = &definition.cost {
            model.cost.input = cost.input;
            model.cost.output = cost.output;
            model.cost.cache_read = cost.cache_read;
            model.cost.cache_write = cost.cache_write;
        }
        model.context_window = definition.context_window.unwrap_or(128_000.0) as u64;
        model.max_tokens = definition.max_tokens.unwrap_or(16_384.0) as u64;
        model.sampling_params = definition.sampling_params.clone();
        model.headers = definition.headers.clone();
        result.push(model);
    }
    Ok(result)
}

/// Extension provider config input (upstream `ProviderConfigInput`, reduced
/// to the model surface the composer consumes).
#[derive(Debug, Clone, Default)]
pub struct ProviderExtensionConfig {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api: Option<String>,
    pub api_key: Option<String>,
    pub auth_header: Option<bool>,
    pub models: Option<Vec<ExtensionModelConfig>>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionModelConfig {
    pub id: String,
    pub name: Option<String>,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub reasoning: Option<bool>,
    pub thinking_level_map: Option<crate::core::model_config::ThinkingLevelMapConfig>,
    pub input: Option<Vec<String>>,
    pub cost: Option<crate::core::model_config::ModelCostConfig>,
    pub context_window: Option<f64>,
    pub max_tokens: Option<f64>,
    pub sampling_params: Option<Value>,
    pub headers: Option<BTreeMap<String, String>>,
}

/// Validate an extension provider config (upstream `validateExtensionProvider`).
pub fn validate_extension_provider(
    provider_id: &str,
    base: &[Model],
    models_config: Option<&ModelsJsonProvider>,
    extension: &ProviderExtensionConfig,
) -> Result<(), String> {
    if extension.api.is_none()
        && extension.models.as_ref().is_some_and(|m| !m.is_empty())
        && extension.api.is_none()
    {
        // streamSimple-only extension providers require api in the JS port;
        // the Rust surface has no streamSimple so the check is informational.
    }
    let layered = apply_models_json(provider_id, base, models_config)?;
    apply_extension(provider_id, &layered, extension)?;
    Ok(())
}

/// Apply a models.json provider config over the bundled provider's models
/// (upstream `applyModelsJson`). `config == None` returns the base models
/// unchanged (a clone).
pub fn apply_models_json(
    provider_id: &str,
    base_models: &[Model],
    config: Option<&ModelsJsonProvider>,
) -> Result<Vec<Model>, String> {
    let Some(config) = config else {
        return Ok(base_models.to_vec());
    };
    if config.oauth.is_some() && config.base_url.is_none() {
        return Err(format!(
            "Provider {provider_id}: \"baseUrl\" is required when \"oauth\" is set."
        ));
    }
    let has_overrides = config
        .model_overrides
        .as_ref()
        .map(|o| !o.is_empty())
        .unwrap_or(false);
    let has_models = config
        .models
        .as_ref()
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if !has_models
        && config.base_url.is_none()
        && config.headers.is_none()
        && config.compat.is_none()
        && !has_overrides
        && config.api_key.is_none()
        && config.oauth.is_none()
        && config.auth_header.is_none()
    {
        return Err(format!(
            "Provider {provider_id}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        ));
    }

    let mut models: Vec<Model> = base_models
        .iter()
        .map(|model| {
            let mut m = model.clone();
            if config.oauth.as_deref() != Some("radius") {
                if let Some(base_url) = &config.base_url {
                    m.base_url = base_url.clone();
                }
            }
            m.compat = merge_compat(m.compat.as_ref(), config.compat.as_ref());
            m
        })
        .collect();

    for definition in config.models.iter().flatten() {
        let existing_index = models.iter().position(|model| model.id == definition.id);
        let defaults = match existing_index {
            Some(index) => Some(models[index].clone()),
            None => models.first().cloned(),
        };
        let model = model_from_json(provider_id, definition, config, defaults.as_ref())?;
        if let Some(index) = existing_index {
            models[index] = model;
        } else {
            models.push(model);
        }
    }
    Ok(models)
}

/// Apply model overrides from models.json after all other layers
/// (upstream `getModels` tail).
pub fn apply_model_overrides(
    models: Vec<Model>,
    config: Option<&ModelsJsonProvider>,
) -> Vec<Model> {
    let Some(config) = config else { return models };
    let Some(overrides) = &config.model_overrides else {
        return models;
    };
    models
        .into_iter()
        .map(|model| {
            if let Some(override_config) = overrides.get(&model.id) {
                apply_model_override(&model, override_config)
            } else {
                model
            }
        })
        .collect()
}

/// Auth status classification for a provider config (upstream
/// `configuredRequestAuthStatus`). Mirrors the `AuthStatus` surface.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthStatus {
    pub configured: bool,
    pub source: Option<&'static str>,
    pub label: Option<String>,
}

pub fn is_command_config_value(value: &str) -> bool {
    value.starts_with('!')
}

/// Env var names referenced by a config value (`$VAR` / `${VAR}`).
pub fn config_value_env_var_names(value: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'{' {
                j += 1;
                let start = j;
                while j < bytes.len() && bytes[j] != b'}' {
                    j += 1;
                }
                if j > start {
                    names.push(value[start..j].to_string());
                }
            } else {
                let start = j;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > start {
                    names.push(value[start..j].to_string());
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    names
}

/// True when the config value references env vars (i.e. is a template).
pub fn is_config_value_configured(value: &str) -> bool {
    value.contains('$')
}

/// Classify the auth status of a provider config (upstream
/// `configuredRequestAuthStatus`).
pub fn configured_request_auth_status(
    config: Option<&ModelsJsonProvider>,
    extension_api_key: Option<&str>,
) -> Option<AuthStatus> {
    let value = extension_api_key.or_else(|| config.and_then(|c| c.api_key.as_deref()))?;
    if is_command_config_value(value) {
        return Some(AuthStatus {
            configured: true,
            source: Some("models_json_command"),
            label: None,
        });
    }
    let names = config_value_env_var_names(value);
    if !names.is_empty() {
        if is_config_value_configured(value) {
            return Some(AuthStatus {
                configured: true,
                source: Some("environment"),
                label: Some(names.join(", ")),
            });
        }
        return Some(AuthStatus {
            configured: false,
            source: None,
            label: None,
        });
    }
    Some(AuthStatus {
        configured: true,
        source: Some(if extension_api_key.is_some() {
            "fallback"
        } else {
            "models_json_key"
        }),
        label: None,
    })
}

/// Resolve configured request headers for a model (port of
/// `resolveCompatibilityRequestConfig` without credential-free header
/// resolution).
pub fn resolve_compatibility_request_config(
    model: &Model,
    config: Option<&ModelsJsonProvider>,
) -> CompatibilityRequestConfig {
    let configured = {
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(config_headers) = config.and_then(|c| c.headers.as_ref()) {
            for (k, v) in config_headers {
                headers.insert(k.clone(), v.clone());
            }
        }
        if let Some(definition) = config
            .and_then(|c| c.models.as_ref())
            .and_then(|models| models.iter().find(|m| m.id == model.id))
        {
            if let Some(model_headers) = &definition.headers {
                for (k, v) in model_headers {
                    headers.insert(k.clone(), v.clone());
                }
            }
        }
        headers
    };
    let mut merged: ProviderHeaders = BTreeMap::new();
    if let Some(model_headers) = &model.headers {
        for (k, v) in model_headers {
            merged.insert(k.clone(), Some(v.clone()));
        }
    }
    for (k, v) in configured {
        merged.insert(k, Some(v));
    }
    CompatibilityRequestConfig {
        headers: if merged.is_empty() {
            None
        } else {
            Some(merged)
        },
        auth_header: config.and_then(|c| c.auth_header).unwrap_or(false),
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompatibilityRequestConfig {
    pub headers: Option<ProviderHeaders>,
    pub auth_header: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model_config::ModelConfig;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> Model {
        let mut m = Model::new(id, id, "openai-responses", provider);
        m.base_url = format!("https://{provider}.example.com/v1");
        m
    }

    #[test]
    fn apply_models_json_none_returns_base() {
        let base = vec![model("demo", "base-1")];
        let out = apply_models_json("demo", &base, None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "base-1");
    }

    #[test]
    fn apply_models_json_overrides_base_url() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "baseUrl": "https://overridden.example.com/v1", "api": "openai-responses" } }
        })).unwrap();
        let out = apply_models_json("demo", &[model("demo", "base-1")], cfg.get_provider("demo"))
            .unwrap();
        assert_eq!(out[0].base_url, "https://overridden.example.com/v1");
    }

    #[test]
    fn apply_models_json_upserts_custom_models() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": {
                "baseUrl": "https://demo.example.com/v1",
                "api": "openai-responses",
                "models": [
                    { "id": "custom-1", "reasoning": true, "cost": { "input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 1.0 } }
                ]
            } }
        })).unwrap();
        let out = apply_models_json("demo", &[model("demo", "base-1")], cfg.get_provider("demo"))
            .unwrap();
        assert_eq!(out.len(), 2);
        let custom = out.iter().find(|m| m.id == "custom-1").unwrap();
        assert!(custom.reasoning);
        assert_eq!(custom.base_url, "https://demo.example.com/v1");
        assert_eq!(custom.cost.input, 1.0);
    }

    #[test]
    fn apply_models_json_custom_model_inherits_defaults() {
        // A custom model without api/baseUrl inherits the provider defaults
        // from the base catalog model (upstream modelFromJson defaults).
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "models": [{ "id": "custom-x", "reasoning": true }] } }
        }))
        .unwrap();
        let out = apply_models_json("demo", &[model("demo", "base-1")], cfg.get_provider("demo"))
            .unwrap();
        let custom = out.iter().find(|m| m.id == "custom-x").unwrap();
        assert_eq!(custom.api, "openai-responses");
        assert_eq!(custom.base_url, "https://demo.example.com/v1");
        assert_eq!(custom.context_window, 128_000);
        assert!(custom.reasoning);
    }

    #[test]
    fn apply_models_json_no_api_errors_without_defaults() {
        // When the catalog has no model to inherit defaults from, a custom
        // model without api errors.
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "models": [{ "id": "custom-x" }] } }
        }))
        .unwrap();
        let err = apply_models_json("demo", &[], cfg.get_provider("demo")).unwrap_err();
        assert!(err.contains("no \"api\" specified"), "{err}");
    }

    #[test]
    fn model_override_applies_fields() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "baseUrl": "https://demo.example.com/v1", "api": "openai-responses",
                "modelOverrides": { "base-1": { "name": "Renamed", "reasoning": true, "maxTokens": 9999 } } } }
        })).unwrap();
        let base = model("demo", "base-1");
        let overridden = apply_model_override(
            &base,
            cfg.get_provider("demo")
                .unwrap()
                .model_overrides
                .as_ref()
                .unwrap()
                .get("base-1")
                .unwrap(),
        );
        assert_eq!(overridden.name, "Renamed");
        assert!(overridden.reasoning);
        assert_eq!(overridden.max_tokens, 9999);
    }

    #[test]
    fn oauth_requires_base_url() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "radius-demo": { "oauth": "radius" } }
        }))
        .unwrap();
        let err = apply_models_json(
            "radius-demo",
            &[model("radius-demo", "auto")],
            cfg.get_provider("radius-demo"),
        )
        .unwrap_err();
        assert!(err.contains("baseUrl"), "{err}");
    }

    #[test]
    fn empty_config_requires_something() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": {} }
        }))
        .unwrap();
        let err = apply_models_json("demo", &[model("demo", "base-1")], cfg.get_provider("demo"))
            .unwrap_err();
        assert!(err.contains("must specify"), "{err}");
    }

    #[test]
    fn auth_status_classification() {
        let cfg: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "apiKey": "$DEMO_KEY" } }
        }))
        .unwrap();
        let status = configured_request_auth_status(cfg.get_provider("demo"), None).unwrap();
        assert!(status.configured);
        assert_eq!(status.source, Some("environment"));

        let cfg2: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "apiKey": "literal-key" } }
        }))
        .unwrap();
        let status = configured_request_auth_status(cfg2.get_provider("demo"), None).unwrap();
        assert_eq!(status.source, Some("models_json_key"));

        let cfg3: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "apiKey": "!secret fetch" } }
        }))
        .unwrap();
        let status = configured_request_auth_status(cfg3.get_provider("demo"), None).unwrap();
        assert_eq!(status.source, Some("models_json_command"));

        let cfg4: ModelConfig = ModelConfig::from_value(json!({
            "providers": { "demo": { "apiKey": "${A}${B}" } }
        }))
        .unwrap();
        let status = configured_request_auth_status(cfg4.get_provider("demo"), None).unwrap();
        assert_eq!(status.source, Some("environment"));
        assert_eq!(status.label.as_deref(), Some("A, B"));
    }

    #[test]
    fn compat_merge_nested_routing() {
        let base = json!({ "openRouterRouting": { "allow_fallbacks": true, "order": ["a"] } });
        let over = json!({ "openRouterRouting": { "order": ["b"] }, "supportsStore": true });
        let merged = merge_compat(Some(&base), Some(&over)).unwrap();
        assert_eq!(merged["supportsStore"], json!(true));
        assert_eq!(merged["openRouterRouting"]["allow_fallbacks"], json!(true));
        assert_eq!(merged["openRouterRouting"]["order"], json!(["b"]));
    }

    #[test]
    fn apply_extension_replaces_models_with_base_url_override() {
        let extension = ProviderExtensionConfig {
            base_url: Some("https://ext.example.com/v1".to_string()),
            models: Some(vec![ExtensionModelConfig {
                id: "ext-model".to_string(),
                name: Some("Ext Model".to_string()),
                reasoning: Some(true),
                input: Some(vec!["text".to_string(), "image".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let base = vec![model("demo", "base-1")];
        let out = apply_extension("demo", &base, &extension).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ext-model");
        assert_eq!(out[0].provider, "demo");
        assert_eq!(out[0].base_url, "https://ext.example.com/v1");
        assert_eq!(
            out[0].api, "openai-responses",
            "defaults inherited from base"
        );
        assert!(out[0].reasoning);
    }

    #[test]
    fn apply_extension_without_models_only_overrides_base_url() {
        let extension = ProviderExtensionConfig {
            base_url: Some("https://elsewhere.example.com/v1".to_string()),
            ..Default::default()
        };
        let base = vec![model("demo", "base-1")];
        let out = apply_extension("demo", &base, &extension).unwrap();
        assert_eq!(out[0].base_url, "https://elsewhere.example.com/v1");
        assert_eq!(out[0].id, "base-1");
    }

    #[test]
    fn validate_extension_provider_composes_layers() {
        let extension = ProviderExtensionConfig::default();
        assert!(
            validate_extension_provider("demo", &[model("demo", "base-1")], None, &extension)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn provider_composer_preserves_deferred_capabilities_for_overlays() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let _core = crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        let provider = models.get_provider("faux").expect("faux provider");
        assert!(provider
            .single_streams
            .as_ref()
            .and_then(|streams| streams.fetch_deferred.as_ref())
            .is_some());
        assert!(provider
            .single_streams
            .as_ref()
            .and_then(|streams| streams.cancel_deferred.as_ref())
            .is_some());

        let mut overlay = provider.models[0].clone();
        overlay.name = "Overlay Faux".to_string();
        let composed = with_composed_models(&provider, vec![overlay]);
        assert_eq!(composed.models[0].name, "Overlay Faux");
        assert!(composed
            .single_streams
            .as_ref()
            .and_then(|streams| streams.fetch_deferred.as_ref())
            .is_some());
        assert!(composed
            .single_streams
            .as_ref()
            .and_then(|streams| streams.cancel_deferred.as_ref())
            .is_some());
    }

    #[test]
    fn config_value_env_var_names_extraction() {
        assert_eq!(config_value_env_var_names("$KEY"), vec!["KEY"]);
        assert_eq!(config_value_env_var_names("${A} and $B"), vec!["A", "B"]);
        assert!(config_value_env_var_names("literal").is_empty());
    }
}
