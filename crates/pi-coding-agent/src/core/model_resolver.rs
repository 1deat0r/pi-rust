//! Model resolution, scoping, and initial selection — port of
//! `packages/coding-agent/src/core/model-resolver.ts`.
//!
//! Pure functions over model lists: exact reference matching, pattern parsing
//! (`provider/model:thinking` with alias-vs-dated preference), model-scope
//! glob resolution, and CLI model resolution (shared with the auth-command
//! port). The runtime/auth boundary is represented by a small `RegistryView`
//! trait so the functions stay testable without a live model runtime.

use pi_ai::model::Model;

use crate::core::model_runtime::default_model_per_provider;

/// Known thinking levels (upstream `isValidThinkingLevel`).
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn is_valid_thinking_level(level: &str) -> bool {
    THINKING_LEVELS.contains(&level)
}

/// Default thinking level (upstream `defaults.ts`).
pub const DEFAULT_THINKING_LEVEL: &str = "medium";

/// A model paired with an optional explicit thinking level
/// (upstream `ScopedModel`).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    pub thinking_level: Option<String>,
}

/// Check if a model ID looks like an alias (no date suffix).
/// Dates are typically in format: -20241022 or -20250929.
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    !id.ends_with(|c: char| c.is_ascii_digit()) || {
        let last = id.len();
        let start = last.saturating_sub(8);
        let suffix = &id[start..last];
        let has_date_pattern = suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit());
        !has_date_pattern
    }
}

/// Find an exact model reference match. Supports either a bare model id or a
/// canonical `provider/modelId` reference. Ambiguous matches are rejected.
pub fn find_exact_model_reference_match(
    model_reference: &str,
    available_models: &[Model],
) -> Option<Model> {
    let trimmed = model_reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_lowercase();

    let canonical_matches: Vec<&Model> = available_models
        .iter()
        .filter(|m| format!("{}/{}", m.provider, m.id).to_lowercase() == normalized)
        .collect();
    if canonical_matches.len() == 1 {
        return Some(canonical_matches[0].clone());
    }
    if canonical_matches.len() > 1 {
        return None;
    }

    if let Some(slash_index) = trimmed.find('/') {
        let provider = trimmed[..slash_index].trim();
        let model_id = trimmed[slash_index + 1..].trim();
        if !provider.is_empty() && !model_id.is_empty() {
            let provider_matches: Vec<&Model> = available_models
                .iter()
                .filter(|m| {
                    m.provider.to_lowercase() == provider.to_lowercase()
                        && m.id.to_lowercase() == model_id.to_lowercase()
                })
                .collect();
            if provider_matches.len() == 1 {
                return Some(provider_matches[0].clone());
            }
            if provider_matches.len() > 1 {
                return None;
            }
        }
    }

    let id_matches: Vec<&Model> = available_models
        .iter()
        .filter(|m| m.id.to_lowercase() == normalized)
        .collect();
    if id_matches.len() == 1 {
        Some(id_matches[0].clone())
    } else {
        None
    }
}

/// Try to match a pattern to a model from the available models list.
/// Exact match first, then partial id/name matching preferring aliases over
/// dated versions (latest first).
pub fn try_match_model(model_pattern: &str, available_models: &[Model]) -> Option<Model> {
    if let Some(exact) = find_exact_model_reference_match(model_pattern, available_models) {
        return Some(exact);
    }

    let pattern = model_pattern.to_lowercase();
    let matches: Vec<&Model> = available_models
        .iter()
        .filter(|m| {
            m.id.to_lowercase().contains(&pattern) || m.name.to_lowercase().contains(&pattern)
        })
        .collect();
    if matches.is_empty() {
        return None;
    }

    let mut aliases: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|m| is_alias(&m.id))
        .collect();
    let mut dated: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|m| !is_alias(&m.id))
        .collect();
    if !aliases.is_empty() {
        aliases.sort_by(|a, b| b.id.cmp(&a.id));
        return Some(aliases[0].clone());
    }
    dated.sort_by(|a, b| b.id.cmp(&a.id));
    Some(dated[0].clone())
}

/// Result of parsing a model pattern (upstream `ParsedModelResult`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedModelResult {
    pub model: Option<Model>,
    /// Thinking level if explicitly specified in pattern.
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
}

/// Build a fallback model for a provider when the requested id is not in the
/// catalog (upstream `buildFallbackModel`).
pub fn build_fallback_model(
    provider: &str,
    model_id: &str,
    available_models: &[Model],
) -> Option<Model> {
    let provider_models: Vec<&Model> = available_models
        .iter()
        .filter(|m| m.provider == provider)
        .collect();
    let base_model = if !provider_models.is_empty() {
        let default_id = default_model_per_provider(provider);
        match default_id {
            Some(default_id) => provider_models
                .iter()
                .find(|m| m.id == default_id)
                .copied()
                .or_else(|| provider_models.first().copied()),
            None => provider_models.first().copied(),
        }
    } else {
        None
    }?;
    let mut model = base_model.clone();
    model.id = model_id.to_string();
    model.name = model_id.to_string();
    Some(model)
}

/// Parse a pattern to extract model and thinking level. Handles models with
/// colons in their IDs (e.g. OpenRouter's `:exacto` suffix).
pub fn parse_model_pattern(
    pattern: &str,
    available_models: &[Model],
    allow_invalid_thinking_level_fallback: bool,
) -> ParsedModelResult {
    if let Some(exact) = try_match_model(pattern, available_models) {
        return ParsedModelResult {
            model: Some(exact),
            thinking_level: None,
            warning: None,
        };
    }

    let Some(last_colon) = pattern.rfind(':') else {
        return ParsedModelResult::default();
    };

    let prefix = &pattern[..last_colon];
    let suffix = &pattern[last_colon + 1..];

    if is_valid_thinking_level(suffix) {
        let result = parse_model_pattern(
            prefix,
            available_models,
            allow_invalid_thinking_level_fallback,
        );
        if result.model.is_some() {
            return ParsedModelResult {
                thinking_level: if result.warning.is_none() {
                    Some(suffix.to_string())
                } else {
                    None
                },
                ..result
            };
        }
        return result;
    }

    if !allow_invalid_thinking_level_fallback {
        // Strict mode (CLI --model parsing): treat it as part of the model id.
        return ParsedModelResult::default();
    }

    let result = parse_model_pattern(
        prefix,
        available_models,
        allow_invalid_thinking_level_fallback,
    );
    if result.model.is_some() {
        return ParsedModelResult {
            model: result.model,
            thinking_level: None,
            warning: Some(format!(
                "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
            )),
        };
    }
    result
}

/// Glob compiler for model scoping patterns (`*`, `?`, `[...]`).
/// A faithful port of minimatch for the pattern subset pi uses.
/// Reused by the package-manager resource resolver for path-pattern
/// include/exclude filtering (upstream `minimatch`).
pub(crate) fn glob_match(pattern: &str, text: &str, nocase: bool) -> bool {
    let p: Vec<char> = if nocase {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };
    let t: Vec<char> = if nocase {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    glob_match_rec(&p, &t)
}

fn glob_match_rec(p: &[char], t: &[char]) -> bool {
    let mut p = p;
    let mut t = t;
    while !p.is_empty() {
        match p[0] {
            '*' => {
                // Collapse consecutive stars.
                while let Some(c) = p.first() {
                    if *c != '*' {
                        break;
                    }
                    p = &p[1..];
                }
                if p.is_empty() {
                    return true;
                }
                // Try matching the rest of the pattern at every suffix of t.
                for i in 0..=t.len() {
                    if glob_match_rec(p, &t[i..]) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if t.is_empty() {
                    return false;
                }
                p = &p[1..];
                t = &t[1..];
            }
            '[' => {
                // Character class: find the closing bracket.
                let mut j = 1;
                let mut negate = false;
                if j < p.len() && (p[j] == '!' || p[j] == '^') {
                    negate = true;
                    j += 1;
                }
                let start = j;
                let mut class_end = None;
                while j < p.len() {
                    if p[j] == ']' {
                        class_end = Some(j);
                        break;
                    }
                    j += 1;
                }
                let Some(class_end) = class_end else {
                    // Unmatched '[': treat as literal.
                    if t.is_empty() || t[0] != '[' {
                        return false;
                    }
                    p = &p[1..];
                    t = &t[1..];
                    continue;
                };
                if t.is_empty() {
                    return false;
                }
                let tc = t[0];
                let mut range_match = false;
                let mut k = start;
                while k < class_end {
                    if k + 2 < class_end && p[k + 1] == '-' {
                        if tc >= p[k] && tc <= p[k + 2] {
                            range_match = true;
                            break;
                        }
                        k += 3;
                    } else {
                        if p[k] == tc {
                            range_match = true;
                            break;
                        }
                        k += 1;
                    }
                }
                if range_match == negate {
                    return false;
                }
                p = &p[class_end + 1..];
                t = &t[1..];
            }
            c => {
                if t.is_empty() || t[0] != c {
                    return false;
                }
                p = &p[1..];
                t = &t[1..];
            }
        }
    }
    t.is_empty()
}

/// Resolve model patterns to actual Model objects (upstream
/// `resolveModelScopeFromModels`). Returns scoped models plus diagnostics.
pub fn resolve_model_scope_from_models(
    patterns: &[String],
    models: &[Model],
) -> (Vec<ScopedModel>, Vec<ModelScopeDiagnostic>) {
    let available_models = models;
    let mut scoped_models: Vec<ScopedModel> = Vec::new();
    let mut diagnostics: Vec<ModelScopeDiagnostic> = Vec::new();

    for pattern in patterns {
        // Check if pattern contains glob characters.
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let colon_idx = pattern.rfind(':');
            let mut glob_pattern = pattern.as_str();
            let mut thinking_level: Option<String> = None;
            if let Some(colon_idx) = colon_idx {
                let suffix = &pattern[colon_idx + 1..];
                if is_valid_thinking_level(suffix) {
                    thinking_level = Some(suffix.to_string());
                    glob_pattern = &pattern[..colon_idx];
                }
            }

            if let Some(exact) = find_exact_model_reference_match(glob_pattern, available_models) {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(&sm.model, &exact))
                {
                    scoped_models.push(ScopedModel {
                        model: exact,
                        thinking_level: thinking_level.clone(),
                    });
                }
                continue;
            }

            let matching_models: Vec<&Model> = available_models
                .iter()
                .filter(|m| {
                    let full_id = format!("{}/{}", m.provider, m.id);
                    glob_match(glob_pattern, &full_id, true)
                        || glob_match(glob_pattern, &m.id, true)
                })
                .collect();

            if matching_models.is_empty() {
                diagnostics.push(ModelScopeDiagnostic {
                    warning: true,
                    code: "no-match".to_string(),
                    message: format!("No models match pattern \"{pattern}\""),
                    pattern: pattern.clone(),
                });
                continue;
            }

            for model in matching_models {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(&sm.model, model))
                {
                    scoped_models.push(ScopedModel {
                        model: model.clone(),
                        thinking_level: thinking_level.clone(),
                    });
                }
            }
            continue;
        }

        let result = parse_model_pattern(pattern, available_models, true);
        if let Some(warning) = &result.warning {
            diagnostics.push(ModelScopeDiagnostic {
                warning: true,
                code: "invalid-thinking-level".to_string(),
                message: warning.clone(),
                pattern: pattern.clone(),
            });
        }
        let Some(model) = result.model else {
            diagnostics.push(ModelScopeDiagnostic {
                warning: true,
                code: "no-match".to_string(),
                message: format!("No models match pattern \"{pattern}\""),
                pattern: pattern.clone(),
            });
            continue;
        };
        if !scoped_models
            .iter()
            .any(|sm| models_are_equal(&sm.model, &model))
        {
            scoped_models.push(ScopedModel {
                model,
                thinking_level: result.thinking_level,
            });
        }
    }

    (scoped_models, diagnostics)
}

pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.id == b.id && a.provider == b.provider
}

/// Diagnostics for model-scope resolution (upstream `ModelScopeDiagnostic`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelScopeDiagnostic {
    pub warning: bool,
    pub code: String,
    pub message: String,
    pub pattern: String,
}

/// Result of resolving a single model from CLI flags (upstream
/// `ResolveCliModelResult`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolveCliModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

/// Minimal registry view needed by `resolve_cli_model`: the model list plus
/// an auth-configured check per provider. Implemented by callers over a live
/// model runtime or a test double.
pub trait RegistryView {
    fn models(&self) -> &[Model];
    fn has_configured_auth(&self, provider: &str) -> bool;
}

impl RegistryView for &[Model] {
    fn models(&self) -> &[Model] {
        self
    }
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.iter().any(|m| m.provider == provider)
    }
}

impl RegistryView for Vec<Model> {
    fn models(&self) -> &[Model] {
        self
    }
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.iter().any(|m| m.provider == provider)
    }
}

/// Resolve a single model from CLI flags (upstream `resolveCliModel`).
///
/// Supports `--provider <provider> --model <pattern>` and
/// `--model <provider>/<pattern>`, fuzzy matching, thinking-level parsing from
/// `pattern:level`, and building fallback custom-model ids scoped to the
/// provider's base model.
#[allow(clippy::too_many_arguments)]
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<&str>,
    registry: &dyn RegistryView,
) -> ResolveCliModelResult {
    let Some(cli_model) = cli_model else {
        return ResolveCliModelResult::default();
    };

    let available_models = registry.models();
    if available_models.is_empty() {
        return ResolveCliModelResult {
            model: None,
            thinking_level: None,
            warning: None,
            error: Some(
                "No models available. Check your installation or add models to models.json."
                    .to_string(),
            ),
        };
    }

    // Case-insensitive canonical provider lookup.
    let canonical_provider = |name: &str| -> Option<String> {
        let lower = name.to_lowercase();
        available_models
            .iter()
            .find(|m| m.provider.to_lowercase() == lower)
            .map(|m| m.provider.clone())
    };

    let mut provider = cli_provider.and_then(canonical_provider);
    if cli_provider.is_some() && provider.is_none() {
        let unknown_provider_error = match cli_provider {
            Some(provider) => format!(
                "Unknown provider \"{provider}\". Use --list-models to see available providers/models."
            ),
            None => "Unknown provider. Use --list-models to see available providers/models.".to_string(),
        };
        return ResolveCliModelResult {
            model: None,
            thinking_level: None,
            warning: None,
            error: Some(unknown_provider_error),
        };
    }

    let mut pattern = cli_model;
    let mut inferred_provider = false;
    if provider.is_none() {
        if let Some(slash_index) = cli_model.find('/') {
            let maybe_provider = &cli_model[..slash_index];
            if let Some(canonical) = canonical_provider(maybe_provider) {
                provider = Some(canonical);
                pattern = &cli_model[slash_index + 1..];
                inferred_provider = true;
            }
        }
    }

    // Bare exact matches without provider inference.
    if provider.is_none() {
        let lower = cli_model.to_lowercase();
        let exact_matches: Vec<&Model> = available_models
            .iter()
            .filter(|m| {
                m.id.to_lowercase() == lower
                    || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
            })
            .collect();
        if exact_matches.len() == 1 {
            return ResolveCliModelResult {
                model: Some(exact_matches[0].clone()),
                ..Default::default()
            };
        }
        if exact_matches.len() > 1 {
            let authenticated: Vec<&Model> = exact_matches
                .iter()
                .copied()
                .filter(|m| registry.has_configured_auth(&m.provider))
                .collect();
            if authenticated.len() == 1 {
                return ResolveCliModelResult {
                    model: Some(authenticated[0].clone()),
                    ..Default::default()
                };
            }
            let mut matches: Vec<String> = exact_matches
                .iter()
                .map(|m| format!("{}/{}", m.provider, m.id))
                .collect();
            matches.sort();
            let auth_hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            return ResolveCliModelResult {
                model: None,
                warning: None,
                thinking_level: None,
                error: Some(format!(
                    "Model \"{cli_model}\" is ambiguous across providers: {}. {auth_hint} Use --provider or provider/model.",
                    matches.join(", ")
                )),
            };
        }
    }

    if cli_provider.is_some() && provider.is_some() {
        let prefix = format!("{}/", provider.clone().unwrap());
        if cli_model.to_lowercase().starts_with(&prefix.to_lowercase()) {
            let p = provider.clone().unwrap();
            pattern = &cli_model[p.len() + 1..];
        }
    }

    let candidates: Vec<Model> = match &provider {
        Some(p) => available_models
            .iter()
            .filter(|m| m.provider == *p)
            .cloned()
            .collect(),
        None => available_models.to_vec(),
    };
    let result = parse_model_pattern(pattern, &candidates, false);

    if let Some(model) = result.model {
        // Inferred-provider fallback to an authenticated raw model-id match.
        if inferred_provider {
            let raw_exact_matches: Vec<&Model> = available_models
                .iter()
                .filter(|m| {
                    m.id.to_lowercase() == cli_model.to_lowercase() && !models_are_equal(m, &model)
                })
                .collect();
            if !raw_exact_matches.is_empty() && !registry.has_configured_auth(&model.provider) {
                let authenticated: Vec<&Model> = raw_exact_matches
                    .iter()
                    .copied()
                    .filter(|m| registry.has_configured_auth(&m.provider))
                    .collect();
                if authenticated.len() == 1 {
                    return ResolveCliModelResult {
                        model: Some(authenticated[0].clone()),
                        ..Default::default()
                    };
                }
            }
        }
        return ResolveCliModelResult {
            model: Some(model),
            thinking_level: result.thinking_level,
            warning: result.warning,
            error: None,
        };
    }

    if inferred_provider {
        let lower = cli_model.to_lowercase();
        let exact = available_models.iter().find(|m| {
            m.id.to_lowercase() == lower
                || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
        });
        if let Some(exact) = exact {
            return ResolveCliModelResult {
                model: Some(exact.clone()),
                ..Default::default()
            };
        }
        let fallback = parse_model_pattern(cli_model, available_models, false);
        if let Some(model) = fallback.model {
            return ResolveCliModelResult {
                model: Some(model),
                thinking_level: fallback.thinking_level,
                warning: fallback.warning,
                error: None,
            };
        }
    }

    if let Some(provider) = &provider {
        // Parse thinking-level suffix from the pattern before building the
        // fallback model, but only when --thinking is not explicitly provided.
        let mut fallback_pattern = pattern;
        let mut fallback_thinking: Option<String> = None;
        if cli_thinking.is_none() {
            if let Some(last_colon) = pattern.rfind(':') {
                let suffix = &pattern[last_colon + 1..];
                if is_valid_thinking_level(suffix) {
                    fallback_pattern = &pattern[..last_colon];
                    fallback_thinking = Some(suffix.to_string());
                }
            }
        }
        if let Some(mut fallback_model) =
            build_fallback_model(provider, fallback_pattern, available_models)
        {
            let requested_thinking = cli_thinking.or(fallback_thinking.as_deref());
            if let Some(level) = requested_thinking {
                if level != "off" {
                    fallback_model.reasoning = true;
                }
            }
            let fallback_warning = match &result.warning {
                Some(warning) => format!(
                    "{warning} Model \"{fallback_pattern}\" not found for provider \"{provider}\". Using custom model id."
                ),
                None => format!(
                    "Model \"{fallback_pattern}\" not found for provider \"{provider}\". Using custom model id."
                ),
            };
            return ResolveCliModelResult {
                model: Some(fallback_model),
                thinking_level: fallback_thinking,
                warning: Some(fallback_warning),
                error: None,
            };
        }
    }

    let display = match &provider {
        Some(p) => format!("{p}/{pattern}"),
        None => cli_model.to_string(),
    };
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning: result.warning,
        error: Some(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, name: Option<&str>) -> Model {
        let mut m = Model::new(id, name.unwrap_or(id), "openai-chat", provider);
        m.base_url = format!("https://{provider}.example.com/v1");
        m
    }

    fn catalog() -> Vec<Model> {
        vec![
            model("anthropic", "claude-sonnet-4-5", Some("Claude 4.5 Sonnet")),
            model(
                "anthropic",
                "claude-sonnet-4-5-20250929",
                Some("Claude 4.5 Sonnet (dated)"),
            ),
            model("anthropic", "claude-opus-4-8", Some("Claude Opus 4.8")),
            model("google", "gemini-3.1-pro-preview", Some("Gemini 3.1 Pro")),
            model("google", "gemini-3.1-flash", Some("Gemini 3.1 Flash")),
            model("openrouter", "zai/glm-5", Some("GLM-5 via router")),
            model("openrouter", "something/else", Some("Other")),
            model("xai", "grok-4.6", Some("Grok 4.6")),
        ]
    }

    #[test]
    fn exact_reference_match_by_canonical() {
        let models = catalog();
        let found = find_exact_model_reference_match("anthropic/claude-opus-4-8", &models).unwrap();
        assert_eq!(found.id, "claude-opus-4-8");
        assert_eq!(found.provider, "anthropic");
    }

    #[test]
    fn exact_reference_match_by_bare_id() {
        let models = catalog();
        let found = find_exact_model_reference_match("grok-4.6", &models).unwrap();
        assert_eq!(found.id, "grok-4.6");
    }

    #[test]
    fn exact_reference_ambiguous_bare_id_rejected() {
        let models = vec![model("a", "same", None), model("b", "same", None)];
        assert!(find_exact_model_reference_match("same", &models).is_none());
    }

    #[test]
    fn try_match_prefers_alias_over_dated() {
        let models = catalog();
        let found = try_match_model("claude-sonnet-4-5", &models).unwrap();
        assert_eq!(found.id, "claude-sonnet-4-5", "alias must win over dated");
    }

    #[test]
    fn try_match_partial_name() {
        let models = catalog();
        let found = try_match_model("gemini-3.1", &models).unwrap();
        assert_eq!(found.id, "gemini-3.1-pro-preview");
    }

    #[test]
    fn parse_pattern_with_thinking_level() {
        let models = catalog();
        let result = parse_model_pattern("grok-4.6:high", &models, true);
        assert_eq!(result.model.unwrap().id, "grok-4.6");
        assert_eq!(result.thinking_level.as_deref(), Some("high"));
    }

    #[test]
    fn parse_pattern_with_colon_in_model_id_openrouter() {
        let models = vec![model("openrouter", "model:exacto", None)];
        let result = parse_model_pattern("model:exacto", &models, true);
        assert_eq!(result.model.unwrap().id, "model:exacto");
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn parse_pattern_invalid_thinking_strict_mode_fails() {
        let models = catalog();
        let result = parse_model_pattern("grok-4.6:bogus", &models, false);
        assert!(result.model.is_none());
    }

    #[test]
    fn parse_pattern_invalid_thinking_scope_mode_warns() {
        let models = catalog();
        let result = parse_model_pattern("grok-4.6:bogus", &models, true);
        assert_eq!(result.model.unwrap().id, "grok-4.6");
        assert!(result.warning.unwrap().contains("Invalid thinking level"));
    }

    #[test]
    fn resolve_cli_model_provider_model() {
        let models = catalog();
        let result = resolve_cli_model(Some("google"), Some("gemini-3.1-flash"), None, &models);
        assert_eq!(result.error, None);
        assert_eq!(result.model.unwrap().id, "gemini-3.1-flash");
    }

    #[test]
    fn resolve_cli_model_infers_provider_from_slash() {
        let models = catalog();
        let result = resolve_cli_model(None, Some("google/gemini-3.1-flash"), None, &models);
        assert_eq!(result.model.unwrap().provider, "google");
    }

    #[test]
    fn resolve_cli_model_unknown_provider() {
        let models = catalog();
        let result = resolve_cli_model(Some("nope"), Some("x"), None, &models);
        assert!(result.error.unwrap().contains("Unknown provider \"nope\""));
    }

    #[test]
    fn resolve_cli_model_builds_fallback_custom_id() {
        let models = catalog();
        let result = resolve_cli_model(Some("xai"), Some("grok-4.6-xyzzy"), None, &models);
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.id, "grok-4.6-xyzzy");
        assert_eq!(model.provider, "xai");
        assert!(result.warning.unwrap().contains("Using custom model id"));
    }

    #[test]
    fn resolve_cli_model_ambiguous_across_providers() {
        let models = vec![model("a", "dup", None), model("b", "dup", None)];
        let result = resolve_cli_model(None, Some("dup"), None, &models);
        assert!(result.error.unwrap().contains("ambiguous"));
    }

    #[test]
    fn resolve_cli_model_no_models() {
        let result = resolve_cli_model(None, Some("x"), None, &Vec::<Model>::new());
        assert!(result.error.unwrap().contains("No models available"));
    }

    #[test]
    fn glob_scope_pattern_matches_provider_ids() {
        let models = catalog();
        let patterns = vec!["anthropic/*".to_string(), "gemini-*".to_string()];
        let (scoped, diagnostics) = resolve_model_scope_from_models(&patterns, &models);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(scoped.len(), 5); // 3 anthropic + 2 gemini
    }

    #[test]
    fn glob_scope_no_match_diagnostic() {
        let models = catalog();
        let patterns = vec!["nope/*".to_string()];
        let (scoped, diagnostics) = resolve_model_scope_from_models(&patterns, &models);
        assert!(scoped.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "no-match");
    }

    #[test]
    fn scope_pattern_with_thinking_level() {
        let models = catalog();
        let patterns = vec!["anthropic/*:high".to_string()];
        let (scoped, _) = resolve_model_scope_from_models(&patterns, &models);
        assert!(!scoped.is_empty());
        assert!(scoped
            .iter()
            .all(|sm| sm.thinking_level.as_deref() == Some("high")));
    }

    #[test]
    fn glob_match_handles_basic_cases() {
        assert!(glob_match("*sonnet*", "claude-sonnet-4-5", true));
        assert!(glob_match(
            "anthropic/*",
            "anthropic/claude-sonnet-4-5",
            true
        ));
        assert!(!glob_match("anthropic/*", "google/gemini", true));
        assert!(glob_match("gemini-?", "gemini-3", true));
        assert!(!glob_match("gemini-?", "gemini-31", true));
        assert!(glob_match("g[ao]*", "goat-4.6", true));
        assert!(!glob_match("g[ao]*", "glm-5", true));
        assert!(glob_match("*", "anything", true));
        assert!(glob_match("a*b*c", "a1b2c", true));
    }

    #[test]
    fn alias_detection() {
        assert!(is_alias("claude-sonnet-4-5"));
        assert!(is_alias("foo-latest"));
        assert!(!is_alias("claude-sonnet-4-5-20250929"));
        assert!(!is_alias("gemini-20241022"));
    }

    #[test]
    fn models_are_equal_by_id_and_provider() {
        let a = model("p", "m", None);
        let b = model("p", "m", Some("different name"));
        assert!(models_are_equal(&a, &b));
    }
}
