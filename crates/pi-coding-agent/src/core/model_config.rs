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

// ---------------------------------------------------------------------------
// ModelConfig
// ---------------------------------------------------------------------------

/// One immutable load of models.json (upstream `ModelConfig`).
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    providers: BTreeMap<String, ModelsJsonProvider>,
    error: Option<String>,
}

impl ModelConfig {
    /// Load models.json from a path. A missing file yields an empty config
    /// with no error (upstream behavior). Structural/schema errors are
    /// captured in `error()`.
    pub fn load(models_json_path: Option<&Path>) -> ModelConfig {
        let Some(models_json_path) = models_json_path else {
            return ModelConfig { providers: BTreeMap::new(), error: None };
        };
        let path = models_json_path.to_path_buf();
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ModelConfig { providers: BTreeMap::new(), error: None }
            }
            Err(e) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    error: Some(format!("Failed to load models.json: {e}\n\nFile: {}", path.display())),
                }
            }
        };

        let stripped = strip_json_comments(strip_bom(&content));
        let parsed: Value = match serde_json::from_str(&stripped) {
            Ok(value) => value,
            Err(e) => {
                return ModelConfig {
                    providers: BTreeMap::new(),
                    error: Some(format!("Failed to parse models.json: {e}\n\nFile: {}", path.display())),
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
    pub fn from_value(value: Value) -> Result<ModelConfig, Vec<(String, String)>> {
        let mut errors: Vec<(String, String)> = Vec::new();
        let object = match value.as_object() {
            Some(object) => object,
            None => {
                return Err(vec![("root".to_string(), "Expected an object with \"providers\".".to_string())]);
            }
        };
        let Some(providers_value) = object.get("providers") else {
            return Err(vec![("providers".to_string(), "Expected required property \"providers\".".to_string())]);
        };
        let providers_object = match providers_value.as_object() {
            Some(map) => map,
            None => {
                return Err(vec![("providers".to_string(), "Expected an object.".to_string())]);
            }
        };
        for (provider_id, provider_value) in providers_object {
            let path = format!("providers.{provider_id}");
            if !provider_value.is_object() {
                errors.push((path, "Expected an object.".to_string()));
                continue;
            }
            if let Some(models) = provider_value.get("models") {
                if !models.is_array() {
                    errors.push((format!("{path}.models"), "Expected an array.".to_string()));
                }
            }
            if let Some(overrides) = provider_value.get("modelOverrides") {
                if !overrides.is_object() {
                    errors.push((format!("{path}.modelOverrides"), "Expected an object.".to_string()));
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        // Full typed parse. A failure here means a field-level schema error;
        // report the first offending path with serde's message.
        match serde_json::from_value::<BTreeMap<String, ModelsJsonProvider>>(providers_value.clone()) {
            Ok(map) => Ok(ModelConfig { providers: map, error: None }),
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
        self.providers.keys().map(|s| s.as_str())
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
mod tests {
    use super::*;
    use std::fs;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-model-config-{name}-{}", uuid::Uuid::new_v4()));
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
    fn loads_providers_with_models() {
        let path = write_tmp("valid", r#"{
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
}"#);
        let cfg = ModelConfig::load(Some(&path));
        assert!(cfg.get_error().is_none(), "unexpected error: {:?}", cfg.get_error());
        let provider = cfg.get_provider("demo").unwrap();
        assert_eq!(provider.name.as_deref(), Some("Demo"));
        let model = &provider.models.as_ref().unwrap()[0];
        assert_eq!(model.id, "demo-1");
        assert_eq!(model.cost.as_ref().unwrap().input, 0.5);
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let path = write_tmp("comments", r#"{
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
}"#);
        let cfg = ModelConfig::load(Some(&path));
        assert!(cfg.get_error().is_none(), "unexpected error: {:?}", cfg.get_error());
        assert!(cfg.get_provider("demo").is_some());
    }

    #[test]
    fn comment_lookalikes_inside_strings_survive() {
        let source = r#"{ "providers": { "demo": { "apiKey": "http://x/not-a-comment", "baseUrl": "https://y" } } }"#;
        let cfg = ModelConfig::from_value(serde_json::from_str(&strip_json_comments(source)).unwrap());
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
        let path = write_tmp("schemas", r#"{ "providers": { "demo": { "models": "not-an-array" } } }"#);
        let cfg = ModelConfig::load(Some(&path));
        let err = cfg.get_error().unwrap();
        assert!(err.contains("Invalid models.json schema"), "{err}");
    }

    #[test]
    fn strips_bom_before_parse() {
        let path = write_tmp("bom", format!("\u{feff}{}", r#"{ "providers": {} }"#).as_str());
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
        assert_eq!(strip_json_comments("{\"s\": \"a,b,\"}"), "{\"s\": \"a,b,\"}");
        assert_eq!(strip_json_comments("{\"s\": \"// not comment\", \"a\": 1}"), "{\"s\": \"// not comment\", \"a\": 1}");
    }
}
