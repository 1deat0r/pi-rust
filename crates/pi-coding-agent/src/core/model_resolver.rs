//! Model resolution, scoping, and initial selection — port of
//! `packages/coding-agent/src/core/model-resolver.ts`.
//!
//! Pure functions over model lists: exact reference matching, pattern parsing
//! (`provider/model:thinking` with alias-vs-dated preference), model-scope
//! glob resolution, and CLI model resolution (shared with the auth-command
//! port). The runtime/auth boundary is represented by a small `RegistryView`
//! trait so the functions stay testable without a live model runtime.

use std::collections::BTreeSet;

use pi_ai::model::Model;
use pi_ai::models::Models;

use crate::core::model_runtime::{default_model_per_provider, DEFAULT_PROVIDER_ORDER};

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
    let bytes = id.as_bytes();
    let has_date_suffix = bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..]
            .iter()
            .all(|byte| byte.is_ascii_digit());
    !has_date_suffix
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

    /// Return the models that can actually be used with the current
    /// credentials. The default implementation keeps existing lightweight
    /// test adapters source-compatible while making the auth gate explicit
    /// for initial model selection.
    fn available_models(&self) -> Vec<Model> {
        self.models()
            .iter()
            .filter(|model| self.has_configured_auth(&model.provider))
            .cloned()
            .collect()
    }
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

/// Stable snapshot of the catalog and provider-auth state used by the
/// synchronous coding-agent resolver. `Models` exposes owned vectors, so the
/// snapshot makes the resolver independent of facade locks and deterministic
/// in tests while preserving the live auth result observed at bootstrap.
#[derive(Debug, Clone, Default)]
pub struct RegistrySnapshot {
    models: Vec<Model>,
    configured_providers: BTreeSet<String>,
}

impl RegistrySnapshot {
    pub fn from_models(models: &Models) -> Self {
        let providers = models.get_providers();
        let configured_providers = providers
            .into_iter()
            .filter(|provider| models.check_auth(&provider.id).is_some())
            .map(|provider| provider.id)
            .collect();
        Self {
            models: models.get_models(None),
            configured_providers,
        }
    }

    #[cfg(test)]
    fn from_parts(models: Vec<Model>, configured_providers: &[&str]) -> Self {
        Self {
            models,
            configured_providers: configured_providers
                .iter()
                .map(|provider| (*provider).to_string())
                .collect(),
        }
    }
}

impl RegistryView for RegistrySnapshot {
    fn models(&self) -> &[Model] {
        &self.models
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        self.configured_providers.contains(provider)
    }
}

/// Resolve a single model from CLI flags (upstream `resolveCliModel`).
///
/// Supports `--provider <provider> --model <pattern>` and
/// `--model <provider>/<pattern>`, fuzzy matching, thinking-level parsing from
/// `pattern:level`, and building fallback custom-model ids scoped to the
/// provider's base model.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
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

/// Result of the upstream `findInitialModel` selection pass.
#[derive(Debug, Clone, PartialEq)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: String,
    /// True only when the thinking level came from an explicit user source
    /// (a model-pattern thinking suffix or a scoped-model override). Settings
    /// defaults and the builtin fallback fill `thinking_level` but leave this
    /// false so callers can order ambient sources (environment) above
    /// defaults without shadowing explicit selections.
    pub thinking_explicit: bool,
    pub fallback_message: Option<String>,
}

/// Inputs to the initial model selection pass. CLI and environment values are
/// kept separate so their precedence remains visible at the call site, while
/// settings provider/model values are intentionally paired as one saved
/// default candidate.
pub struct InitialModelOptions<'a> {
    pub cli_provider: Option<&'a str>,
    pub cli_model: Option<&'a str>,
    pub env_provider: Option<&'a str>,
    pub env_model: Option<&'a str>,
    pub scoped_models: &'a [ScopedModel],
    pub is_continuing: bool,
    pub default_provider: Option<&'a str>,
    pub default_model_id: Option<&'a str>,
    pub default_thinking_level: Option<&'a str>,
    pub registry: &'a dyn RegistryView,
}

fn initial_model_result(
    model: Option<Model>,
    thinking_level: Option<&str>,
    thinking_explicit: bool,
) -> InitialModelResult {
    InitialModelResult {
        model,
        thinking_level: thinking_level.unwrap_or(DEFAULT_THINKING_LEVEL).to_string(),
        thinking_explicit,
        fallback_message: None,
    }
}

fn canonical_provider(registry: &dyn RegistryView, requested: &str) -> Option<String> {
    registry
        .models()
        .iter()
        .find(|model| model.provider.eq_ignore_ascii_case(requested))
        .map(|model| model.provider.clone())
}

fn default_model_for_provider(registry: &dyn RegistryView, requested: &str) -> Option<Model> {
    let provider = canonical_provider(registry, requested)?;
    let mut provider_models = registry
        .models()
        .iter()
        .filter(|model| model.provider == provider);
    if let Some(default_id) = default_model_per_provider(&provider) {
        if let Some(model) = provider_models.clone().find(|model| model.id == default_id) {
            return Some(model.clone());
        }
    }
    provider_models.next().cloned()
}

/// Find the initial model using the same observable ordering as upstream
/// `findInitialModel`:
///
/// 1. explicit CLI values, then explicit environment values;
/// 2. the first scoped model for a new session;
/// 3. a paired saved provider/model only when that provider is authenticated;
/// 4. the first authenticated provider's default model, then its first model;
/// 5. no model when no provider has usable credentials.
pub fn find_initial_model(options: InitialModelOptions<'_>) -> Result<InitialModelResult, String> {
    let explicit_provider = options.cli_provider.or(options.env_provider);
    let explicit_model = options.cli_model.or(options.env_model);

    // Explicit selection is allowed to target a provider without ambient
    // auth: callers may supply --api-key at request time. It must therefore
    // resolve against the complete catalog, not only authenticated models.
    if explicit_provider.is_some() || explicit_model.is_some() {
        if let Some(model_id) = explicit_model {
            let resolved =
                resolve_cli_model(explicit_provider, Some(model_id), None, options.registry);
            if let Some(error) = resolved.error {
                return Err(error);
            }
            if let Some(model) = resolved.model {
                let thinking_explicit = resolved.thinking_level.is_some();
                return Ok(initial_model_result(
                    Some(model),
                    resolved.thinking_level.as_deref(),
                    thinking_explicit,
                ));
            }
        } else if let Some(provider) = explicit_provider {
            let canonical = canonical_provider(options.registry, provider).ok_or_else(|| {
                format!(
                    "Unknown provider \"{provider}\". Use --list-models to see available providers/models."
                )
            })?;
            let model = default_model_for_provider(options.registry, &canonical).ok_or_else(|| {
                format!(
                    "Provider {canonical:?} has no models cataloged (check the bundled model catalog)"
                )
            })?;
            return Ok(initial_model_result(Some(model), None, false));
        }

        return Err("No model selected. Use --list-models to see available models.".to_string());
    }

    // Scoped models are produced from the available catalog by the caller;
    // preserve their order and thinking override for new sessions.
    if !options.is_continuing {
        if let Some(scoped) = options.scoped_models.first() {
            let thinking_explicit = scoped.thinking_level.is_some();
            return Ok(initial_model_result(
                Some(scoped.model.clone()),
                scoped
                    .thinking_level
                    .as_deref()
                    .or(options.default_thinking_level),
                thinking_explicit,
            ));
        }
    }

    // A saved default is valid only as a pair and only while its provider is
    // authenticated. This prevents a stale Google default from shadowing an
    // available Qwen token-plan credential, which was the observed parity
    // mismatch.
    if let (Some(provider), Some(model_id)) = (options.default_provider, options.default_model_id) {
        if let Some(model) = options
            .registry
            .models()
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
            .filter(|model| options.registry.has_configured_auth(&model.provider))
        {
            return Ok(initial_model_result(
                Some(model.clone()),
                options.default_thinking_level,
                false,
            ));
        }
    }

    let available_models = options.registry.available_models();
    for provider in DEFAULT_PROVIDER_ORDER {
        let Some(default_id) = default_model_per_provider(provider) else {
            continue;
        };
        if let Some(model) = available_models
            .iter()
            .find(|model| model.provider == *provider && model.id == default_id)
        {
            return Ok(initial_model_result(Some(model.clone()), None, false));
        }
    }

    Ok(initial_model_result(
        available_models.into_iter().next(),
        None,
        false,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    fn initial_options<'a>(
        registry: &'a RegistrySnapshot,
        default_provider: Option<&'a str>,
        default_model_id: Option<&'a str>,
    ) -> InitialModelOptions<'a> {
        InitialModelOptions {
            cli_provider: None,
            cli_model: None,
            env_provider: None,
            env_model: None,
            scoped_models: &[],
            is_continuing: false,
            default_provider,
            default_model_id,
            default_thinking_level: None,
            registry,
        }
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
    fn resolve_cli_model_provider_case_variation() {
        let models = catalog();
        let result = resolve_cli_model(Some("GOOGLE"), Some("GEMINI-3.1-FLASH"), None, &models);
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.provider, "google");
        assert_eq!(model.id, "gemini-3.1-flash");
    }

    #[test]
    fn resolve_cli_model_provider_prefixed_case_variation() {
        let models = catalog();
        let result = resolve_cli_model(
            Some("Google"),
            Some("GOOGLE/gemini-3.1-flash"),
            None,
            &models,
        );
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.provider, "google");
        assert_eq!(model.id, "gemini-3.1-flash");
    }

    #[test]
    fn resolve_cli_model_cross_provider_model_warns_and_falls_back() {
        let models = catalog();
        let result = resolve_cli_model(Some("xai"), Some("gemini-3.1-flash"), None, &models);
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.provider, "xai");
        assert_eq!(model.id, "gemini-3.1-flash");
        assert!(result
            .warning
            .unwrap()
            .contains("Model \"gemini-3.1-flash\" not found for provider \"xai\""));
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
    fn resolve_cli_model_bare_model_exact_no_provider() {
        let models = catalog();
        let result = resolve_cli_model(None, Some("grok-4.6"), None, &models);
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.provider, "xai");
        assert_eq!(model.id, "grok-4.6");
    }

    #[test]
    fn resolve_cli_model_fuzzy_partial_matches_alias() {
        let models = catalog();
        let result = resolve_cli_model(Some("anthropic"), Some("opus"), None, &models);
        assert_eq!(result.error, None);
        assert_eq!(result.model.unwrap().id, "claude-opus-4-8");
    }

    #[test]
    fn resolve_cli_model_thinking_suffix_returns_level() {
        let models = catalog();
        let result = resolve_cli_model(Some("xai"), Some("grok-4.6:high"), None, &models);
        assert_eq!(result.error, None);
        assert_eq!(result.model.unwrap().id, "grok-4.6");
        assert_eq!(result.thinking_level.as_deref(), Some("high"));
    }

    #[test]
    fn resolve_cli_model_unknown_model_no_provider_errors() {
        let models = catalog();
        let result = resolve_cli_model(None, Some("nope-xyz"), None, &models);
        let error = result.error.unwrap();
        assert!(error.contains("Model \"nope-xyz\" not found"), "{error}");
        assert!(result.model.is_none());
    }

    #[test]
    fn resolve_cli_model_resolves_model_without_configured_auth() {
        // Upstream resolves against *all* models, not just authenticated
        // providers, so `--api-key` first-time setup keeps working.
        let registry = RegistrySnapshot::from_parts(catalog(), &[]);
        let result = resolve_cli_model(None, Some("google/gemini-3.1-flash"), None, &registry);
        assert_eq!(result.error, None);
        let model = result.model.unwrap();
        assert_eq!(model.provider, "google");
        assert_eq!(model.id, "gemini-3.1-flash");
    }

    #[test]
    fn resolve_cli_model_no_models() {
        let result = resolve_cli_model(None, Some("x"), None, &Vec::<Model>::new());
        assert!(result.error.unwrap().contains("No models available"));
    }

    #[test]
    fn find_initial_model_uses_authenticated_saved_default_pair() {
        let registry = RegistrySnapshot::from_parts(
            vec![model("google", "gemini-3.1-pro-preview", None)],
            &["google"],
        );
        let result = find_initial_model(initial_options(
            &registry,
            Some("google"),
            Some("gemini-3.1-pro-preview"),
        ))
        .unwrap();
        assert_eq!(
            result
                .model
                .as_ref()
                .map(|model| (model.provider.as_str(), model.id.as_str())),
            Some(("google", "gemini-3.1-pro-preview"))
        );
    }

    #[test]
    fn find_initial_model_skips_unauthenticated_saved_default() {
        let registry = RegistrySnapshot::from_parts(
            vec![
                model("google", "gemini-3.1-pro-preview", None),
                model("qwen-token-plan", "qwen3.7-max", None),
            ],
            &["qwen-token-plan"],
        );
        let result = find_initial_model(initial_options(
            &registry,
            Some("google"),
            Some("gemini-3.1-pro-preview"),
        ))
        .unwrap();
        let model = result.model.unwrap();
        assert_eq!(model.provider, "qwen-token-plan");
        assert_eq!(model.id, "qwen3.7-max");
    }

    #[test]
    fn find_initial_model_uses_the_only_authenticated_provider_default() {
        let registry = RegistrySnapshot::from_parts(
            vec![
                model("google", "gemini-3.1-pro-preview", None),
                model("qwen-token-plan", "qwen3.7-max", None),
            ],
            &["qwen-token-plan"],
        );
        let result = find_initial_model(initial_options(&registry, None, None)).unwrap();
        let model = result.model.unwrap();
        assert_eq!(model.provider, "qwen-token-plan");
        assert_eq!(model.id, "qwen3.7-max");
    }

    #[test]
    fn find_initial_model_returns_no_model_without_credentials() {
        let registry = RegistrySnapshot::from_parts(
            vec![model("google", "gemini-3.1-pro-preview", None)],
            &[],
        );
        let result = find_initial_model(initial_options(&registry, None, None)).unwrap();
        assert!(result.model.is_none());
        assert_eq!(result.thinking_level, DEFAULT_THINKING_LEVEL);
    }

    #[test]
    fn find_initial_model_cli_values_override_environment_and_saved_defaults() {
        let registry = RegistrySnapshot::from_parts(
            vec![
                model("google", "gemini-3.1-pro-preview", None),
                model("qwen-token-plan", "qwen3.7-max", None),
            ],
            &[],
        );
        let mut options =
            initial_options(&registry, Some("google"), Some("gemini-3.1-pro-preview"));
        options.env_provider = Some("google");
        options.env_model = Some("google/gemini-3.1-pro-preview");
        options.cli_provider = Some("qwen-token-plan");
        options.cli_model = Some("qwen3.7-max");
        let result = find_initial_model(options).unwrap();
        let model = result.model.unwrap();
        assert_eq!(model.provider, "qwen-token-plan");
        assert_eq!(model.id, "qwen3.7-max");
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
