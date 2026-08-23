//! Model runtime resolution — the coding-agent-side selection over the pi-ai
//! Models facade. Mirrors `packages/coding-agent/src/core/model-resolver.ts`
//! for the one-shot run path: provider/model id resolution with a
//! `provider/model:thinking`-style hint, catalog glob-scoped lookup, and the
//! upstream per-provider default model table.

use pi_ai::model::Model;
use pi_ai::models::Models;

/// Default model IDs per provider (upstream `defaultModelPerProvider`).
pub fn default_model_per_provider(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "amazon-bedrock" => "us.anthropic.claude-opus-4-6-v1",
        "ant-ling" => "Ring-2.6-1T",
        "anthropic" => "claude-opus-4-8",
        "openai" => "gpt-5.5",
        "azure-openai-responses" => "gpt-5.4",
        "openai-codex" => "gpt-5.5",
        "nvidia" => "nvidia/nemotron-3-super-120b-a12b",
        "deepseek" => "deepseek-v4-pro",
        "google" => "gemini-3.1-pro-preview",
        "google-vertex" => "gemini-3.1-pro-preview",
        "github-copilot" => "gpt-5.4",
        "openrouter" => "moonshotai/kimi-k2.6",
        "vercel-ai-gateway" => "zai/glm-5.1",
        "xai" => "grok-4.6",
        "groq" => "openai/gpt-oss-120b",
        "cerebras" => "gpt-oss-120b",
        "zai" => "glm-5.3",
        "zai-coding-cn" => "glm-5.3",
        "mistral" => "devstral-medium-latest",
        "minimax" => "MiniMax-M2.7",
        "minimax-cn" => "MiniMax-M2.7",
        "moonshotai" => "kimi-k2.6",
        "moonshotai-cn" => "kimi-k2.6",
        "huggingface" => "moonshotai/Kimi-K2.6",
        "fireworks" => "accounts/fireworks/models/kimi-k2p6",
        "together" => "moonshotai/Kimi-K2.6",
        "baseten" => "zai-org/GLM-5.2",
        "opencode" => "kimi-k2.6",
        "opencode-go" => "kimi-k2.6",
        "kimi-coding" => "kimi-for-coding",
        "cloudflare-workers-ai" => "@cf/moonshotai/kimi-k2.6",
        "cloudflare-ai-gateway" => "workers-ai/@cf/moonshotai/kimi-k2.6",
        "qwen-token-plan" => "qwen3.7-max",
        "qwen-token-plan-cn" => "qwen3.7-max",
        "qwen-token-plan-individual" => "qwen3.8-max",
        "xiaomi" => "mimo-v2.5-pro",
        "xiaomi-token-plan-cn" => "mimo-v2.5-pro",
        "xiaomi-token-plan-ams" => "mimo-v2.5-pro",
        "xiaomi-token-plan-sgp" => "mimo-v2.5-pro",
        _ => return None,
    })
}

/// Strip a `provider/` prefix and `:thinking` suffix from a model hint
/// (upstream pattern parsing for the run path).
pub fn parse_model_hint(hint: &str) -> (String, Option<String>) {
    let hint = hint.trim();
    // Split off a :thinking suffix only if it names a valid thinking level.
    let (base, thinking) = if let Some((base, suffix)) = hint.rsplit_once(':') {
        let known = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        if known.contains(&suffix) {
            (base.to_string(), Some(suffix.to_string()))
        } else {
            (hint.to_string(), None)
        }
    } else {
        (hint.to_string(), None)
    };
    // Strip a provider/ prefix.
    let base = match base.split_once('/') {
        Some((_provider, id)) if !id.is_empty() => id.to_string(),
        _ => base,
    };
    (base, thinking)
}

/// Resolve a concrete model for a provider (upstream `resolveModel` for the
/// one-shot path): exact id match first, then catalog substring, then the
/// per-provider default, then the provider's first model.
pub fn resolve_run_model_for_provider(
    models: &Models,
    provider: &str,
    hint: Option<&str>,
) -> Result<Model, String> {
    let provider_models = models.get_models(Some(provider));
    if provider_models.is_empty() {
        return Err(format!(
            "Provider {provider:?} has no models cataloged (check the bundled model catalog)"
        ));
    }
    if let Some(hint) = hint {
        if !hint.trim().is_empty() {
            let (base, thinking) = parse_model_hint(hint);
            // Exact id match.
            if let Some(model) = provider_models.iter().find(|m| m.id == base) {
                return Ok(model.clone());
            }
            // Catalog substring match (upstream glob/fuzzy).
            let lower = base.to_lowercase();
            if let Some(model) = provider_models
                .iter()
                .find(|m| m.id.to_lowercase().contains(&lower))
            {
                return Ok(model.clone());
            }
            let _ = thinking;
            return Err(format!(
                "Unknown model {base:?} for provider {provider:?} (available: {} models; use `pi --list-models {provider}` to search)",
                provider_models.len()
            ));
        }
    }
    // Default model per provider.
    if let Some(default_id) = default_model_per_provider(provider) {
        if let Some(model) = provider_models.iter().find(|m| m.id == default_id) {
            return Ok(model.clone());
        }
    }
    // First available.
    provider_models
        .first()
        .cloned()
        .ok_or_else(|| format!("Provider {provider:?} has no models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_provider_prefix_and_thinking() {
        let (id, thinking) = parse_model_hint("google/gemini-2.5-pro");
        assert_eq!(id, "gemini-2.5-pro");
        assert_eq!(thinking, None);
        let (id, thinking) = parse_model_hint("gemini-2.5-pro:high");
        assert_eq!(id, "gemini-2.5-pro");
        assert_eq!(thinking.as_deref(), Some("high"));
        let (id, thinking) = parse_model_hint("openrouter/some/model:exacto:low");
        assert_eq!(id, "some/model:exacto");
        assert_eq!(thinking.as_deref(), Some("low"));
    }

    fn test_models() -> Models {
        // Facade with a real google provider (catalog models, no key needed
        // for resolution).
        let p = pi_ai::providers::google_provider_real();
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        models.set_provider(p);
        models
    }

    #[test]
    fn exact_model_resolution() {
        let models = test_models();
        let model =
            resolve_run_model_for_provider(&models, "google", Some("gemini-2.5-flash")).unwrap();
        assert_eq!(model.id, "gemini-2.5-flash");
        assert_eq!(model.provider, "google");
    }

    #[test]
    fn default_model_resolution() {
        let models = test_models();
        let model = resolve_run_model_for_provider(&models, "google", None).unwrap();
        assert_eq!(model.id, "gemini-3.1-pro-preview");
    }

    #[test]
    fn unknown_model_errors() {
        let models = test_models();
        let err =
            resolve_run_model_for_provider(&models, "google", Some("does-not-exist")).unwrap_err();
        assert!(err.contains("Unknown model"), "{err}");
    }

    #[test]
    fn unknown_provider_errors() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let err = resolve_run_model_for_provider(&models, "nope", None).unwrap_err();
        assert!(err.contains("no models"), "{err}");
    }

    #[test]
    fn default_table_covers_primary_providers() {
        for p in [
            "google",
            "openai",
            "anthropic",
            "xai",
            "deepseek",
            "groq",
            "mistral",
            "openrouter",
        ] {
            assert!(
                default_model_per_provider(p).is_some(),
                "{p} missing default"
            );
        }
    }
}
