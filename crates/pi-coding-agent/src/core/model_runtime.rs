//! Model runtime resolution — the coding-agent-side selection over the pi-ai
//! Models facade. Mirrors `packages/coding-agent/src/core/model-resolver.ts`
//! for the one-shot run path: provider/model id resolution with a
//! `provider/model:thinking`-style hint, catalog glob-scoped lookup, and the
//! upstream per-provider default model table.

use std::sync::Arc;

use pi_ai::auth::{ApiKeyAuth, AuthCheck, AuthContext, AuthResult, ModelAuth, ProviderAuth};
use pi_ai::event_stream::AssistantMessageEventStream;
use pi_ai::model::Model;
use pi_ai::models::{
    create_provider, DeferredCancelOptions, DeferredFetchOptions, Models, ModelsRefreshOptions,
    ProviderApiSpec, ProviderStreams,
};
use pi_ai::providers::{FauxProviderCore, RegisterFauxProviderOptions};
use pi_ai::types::{AssistantMessage, Context, DeferredHandle, SimpleStreamOptions, StreamOptions};

/// Provider iteration order used by upstream `findInitialModel` when it
/// chooses among authenticated providers. Keep this separate from the
/// catalog's registration order: the default-model table is the selection
/// oracle, not the map order of the runtime facade.
pub const DEFAULT_PROVIDER_ORDER: &[&str] = &[
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "openai",
    "azure-openai-responses",
    "openai-codex",
    "radius",
    "nvidia",
    "deepseek",
    "google",
    "google-vertex",
    "github-copilot",
    "openrouter",
    "vercel-ai-gateway",
    "xai",
    "groq",
    "cerebras",
    "zai",
    "zai-coding-cn",
    "mistral",
    "minimax",
    "minimax-cn",
    "moonshotai",
    "moonshotai-cn",
    "huggingface",
    "fireworks",
    "together",
    "baseten",
    "opencode",
    "opencode-go",
    "kimi-coding",
    "cloudflare-workers-ai",
    "cloudflare-ai-gateway",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "xiaomi",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-sgp",
];

/// Refresh a persisted OAuth credential before a network-backed turn. The
/// low-level `pi-ai::Models` facade is intentionally synchronous in this Rust
/// port, so the coding-agent boundary performs the upstream five-minute
/// refresh/persist operation before handing the request to that facade.
pub async fn refresh_provider_oauth_if_needed(
    models: &Models,
    provider_id: &str,
) -> Result<(), String> {
    let Some(provider) = models.get_provider(provider_id) else {
        return Ok(());
    };
    let Some(oauth) = provider.auth.oauth else {
        return Ok(());
    };
    let storage = crate::core::auth_storage::AuthStorage::create(crate::config::get_auth_path());
    crate::core::auth_storage::refresh_oauth_credential_in_storage(
        &storage,
        provider_id,
        oauth,
        None,
        None,
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("OAuth refresh failed for {provider_id}: {error}"))
}

/// Register and refresh the native llama.cpp provider when it is explicitly
/// selected.  The provider is intentionally lazy: normal Pi startup does not
/// contact localhost or Hugging Face, while `--provider llama.cpp` and an
/// existing local catalog use the same real Models refresh boundary as every
/// other dynamic provider.
pub async fn register_llama_provider_if_selected(
    models: &Models,
    provider_id: &str,
    allow_network: bool,
) -> Result<(), String> {
    if !provider_id.eq_ignore_ascii_case(crate::core::llama::LLAMA_PROVIDER_ID) {
        return Ok(());
    }

    let controller = crate::core::llama::LlamaProviderController::new();
    controller.register_into(models);
    let result = models
        .refresh(ModelsRefreshOptions {
            allow_network,
            providers: Some(vec![crate::core::llama::LLAMA_PROVIDER_ID.to_owned()]),
            force: false,
            signal: None,
        })
        .await;
    if let Some((_, error)) = result.errors.into_iter().next() {
        return Err(format!("llama.cpp model catalog refresh failed: {error}"));
    }
    Ok(())
}

/// The coding-agent-facing runtime facade. It keeps the auth-applied
/// `pi-ai::Models` collection as the single dispatch seam for normal streams,
/// simple streams, deferred resolution, and cancellation.
#[derive(Clone)]
pub struct ModelRuntime {
    models: Models,
}

impl ModelRuntime {
    pub fn new(models: Models) -> Self {
        Self { models }
    }

    pub fn models(&self) -> Models {
        self.models.clone()
    }

    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.models.stream(model, context, options)
    }

    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.models.stream_simple(model, context, options)
    }

    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessage {
        self.models.complete_simple(model, context, options).await
    }

    pub async fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&DeferredFetchOptions>,
    ) -> Result<AssistantMessage, pi_ai::models::ModelsError> {
        self.models.fetch_deferred(model, handle, options).await
    }

    pub async fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&DeferredCancelOptions>,
    ) -> Result<(), pi_ai::models::ModelsError> {
        self.models.cancel_deferred(model, handle, options).await
    }
}

/// Auth implementation for the deterministic faux provider. Faux ignores the
/// key, but facade-backed calls still need a configured provider to exercise
/// the same `Models::apply_auth` path as production providers.
struct FauxApiKeyAuth;

impl ApiKeyAuth for FauxApiKeyAuth {
    fn name(&self) -> &str {
        "Faux API key"
    }

    fn check(
        &self,
        _ctx: &AuthContext,
        _credential: Option<&pi_ai::auth::ApiKeyCredential>,
    ) -> Option<AuthCheck> {
        Some(AuthCheck {
            source: Some("faux".to_string()),
            auth_type: "api_key",
        })
    }

    fn resolve(
        &self,
        _ctx: &AuthContext,
        _credential: Option<&pi_ai::auth::ApiKeyCredential>,
    ) -> Option<AuthResult> {
        Some(AuthResult {
            auth: ModelAuth {
                api_key: Some("faux-key".to_string()),
                ..Default::default()
            },
            env: None,
            source: Some("faux".to_string()),
        })
    }
}

/// Register a faux provider with every stream capability, including deferred
/// fetch/cancel. Returning the core lets callers seed deterministic responses
/// while all subsequent operations travel through the shared Models facade.
pub fn register_faux_provider(
    models: &Models,
    options: &RegisterFauxProviderOptions,
) -> FauxProviderCore {
    let core = FauxProviderCore::new(options);

    let stream_core = core.clone();
    let stream = Arc::new(
        move |model: &Model, context: &Context, options: Option<&StreamOptions>| {
            let simple_options = options.map(|options| SimpleStreamOptions {
                base: options.clone(),
                ..Default::default()
            });
            stream_core.stream(model, context, simple_options.as_ref())
        },
    );

    let simple_core = core.clone();
    let stream_simple = Arc::new(
        move |model: &Model, context: &Context, options: Option<&SimpleStreamOptions>| {
            simple_core.stream(model, context, options)
        },
    );

    let fetch_core = core.clone();
    let fetch_deferred = Arc::new(
        move |model: &Model, handle: &DeferredHandle, _options: &DeferredFetchOptions| {
            fetch_core.fetch_deferred_stream(model, handle, None)
        },
    );

    let cancel_core = core.clone();
    let cancel_deferred = Arc::new(
        move |_model: &Model, handle: &DeferredHandle, _options: &DeferredCancelOptions| {
            let core = cancel_core.clone();
            let handle = handle.clone();
            Box::pin(async move { core.cancel_deferred(&handle).await })
                as pi_ai::models::DeferredCancelFuture
        },
    );

    models.set_provider(create_provider(pi_ai::models::CreateProviderOptions {
        id: core.provider.clone(),
        name: Some("Faux".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(FauxApiKeyAuth)),
            oauth: None,
        },
        models: core.models.clone(),
        api: ProviderApiSpec::Single(ProviderStreams {
            stream,
            stream_simple,
            fetch_deferred: Some(fetch_deferred),
            cancel_deferred: Some(cancel_deferred),
        }),
        filter_models: None,
    }));
    core
}

/// Default model IDs per provider (upstream `defaultModelPerProvider`).
pub fn default_model_per_provider(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "amazon-bedrock" => "us.anthropic.claude-opus-4-6-v1",
        "ant-ling" => "Ring-2.6-1T",
        "anthropic" => "claude-opus-4-8",
        "openai" => "gpt-5.5",
        "azure-openai-responses" => "gpt-5.4",
        "openai-codex" => "gpt-5.5",
        "radius" => "auto",
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
    // A canonical `provider/model` hint is an explicit model selection even
    // when the caller resolved its provider before the model catalog was
    // available. Redirect it to that registered provider rather than trying
    // to parse the model id inside the currently selected provider. This is
    // the run-path equivalent of upstream `resolveCliModel`'s provider
    // inference and preserves model flags when auth-based bootstrap changes
    // the ambient provider.
    if let Some(hint) = hint {
        let trimmed = hint.trim();
        if let Some((hint_provider, _)) = trimmed.split_once('/') {
            if let Some(canonical_provider) = models
                .get_providers()
                .into_iter()
                .find(|candidate| candidate.id.eq_ignore_ascii_case(hint_provider))
                .map(|candidate| candidate.id)
            {
                if !canonical_provider.eq_ignore_ascii_case(provider) {
                    return resolve_run_model_for_provider(
                        models,
                        &canonical_provider,
                        Some(trimmed),
                    );
                }
            }
        }
    }
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::providers::{faux_assistant_message, FauxAssistantOptions, FauxResponseStep};
    use pi_ai::types::{ContentBlock, DeferredOption, StopReason};

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
            "radius",
        ] {
            assert!(
                default_model_per_provider(p).is_some(),
                "{p} missing default"
            );
        }
        assert_eq!(default_model_per_provider("radius"), Some("auto"));
    }

    #[tokio::test]
    async fn deferred_runtime_submits_resolves_and_cancels() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let core = register_faux_provider(
            &models,
            &RegisterFauxProviderOptions {
                deferred: Some(pi_ai::providers::FauxDeferredOptions {
                    pending_fetches: Some(1),
                    poll_after_ms: Some(5),
                }),
                ..Default::default()
            },
        );
        core.set_responses(vec![
            FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("ready")],
                FauxAssistantOptions::default(),
            )),
            FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("cancelled")],
                FauxAssistantOptions::default(),
            )),
        ]);

        let runtime = ModelRuntime::new(models.clone());
        let model = models.get_model("faux", "faux-1").expect("faux model");
        let deferred_options = SimpleStreamOptions {
            deferred: Some(DeferredOption::Bool(true)),
            ..Default::default()
        };
        let submission = runtime
            .complete_simple(&model, &Context::default(), Some(&deferred_options))
            .await;
        assert_eq!(submission.stop_reason(), Some(StopReason::Deferred));
        let handle = submission.deferred().cloned().expect("deferred handle");

        let pending = runtime
            .fetch_deferred(&model, &handle, None)
            .await
            .expect("deferred poll");
        assert_eq!(pending.stop_reason(), Some(StopReason::Deferred));
        let ready = runtime
            .fetch_deferred(
                &model,
                &handle,
                Some(&DeferredFetchOptions {
                    wait: Some(0),
                    ..Default::default()
                }),
            )
            .await
            .expect("deferred resolution");
        assert_eq!(ready.stop_reason(), Some(StopReason::Stop));

        let cancelled_submission = runtime
            .complete_simple(&model, &Context::default(), Some(&deferred_options))
            .await;
        let cancelled_handle = cancelled_submission
            .deferred()
            .cloned()
            .expect("second deferred handle");
        runtime
            .cancel_deferred(&model, &cancelled_handle, None)
            .await
            .expect("deferred cancellation");
        let cancelled = runtime
            .fetch_deferred(&model, &cancelled_handle, None)
            .await
            .expect("cancellation is an in-band provider result");
        assert_eq!(cancelled.stop_reason(), Some(StopReason::Error));
        assert!(cancelled
            .error_message()
            .unwrap_or_default()
            .contains("cancelled"));
        assert_eq!(
            core.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .deferred_fetch_count,
            3
        );
        assert_eq!(
            core.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .cancelled_deferred
                .len(),
            1
        );
    }
}
