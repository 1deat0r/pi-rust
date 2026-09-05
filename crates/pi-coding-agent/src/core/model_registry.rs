//! Model registry — port of `packages/coding-agent/src/core/model-registry.ts`
//! (the synchronous compatibility facade).
//!
//! Composes with:
//! - the pi-ai `Models` facade (crate `pi-ai/src/models.rs`) for the live
//!   provider stream/auth surface, and
//! - the on-disk `~/.pi/agent/models.json` overlay via `ModelConfig`
//!   (merge-over-bundled-catalog behavior from provider-composer.ts).
//!
//! The registry presents the merged catalog (`builtin` + `models.json`
//! overlay) as the `get_all` surface used by extension-facing code, the
//! resolver, and `--list-models`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use pi_ai::auth::{
    decorate_oauth_auth, ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction,
    AuthResult, ModelAuth, OAuthCredential, ProviderAuth,
};
use pi_ai::error::PiAiError;
use pi_ai::model::Model;
use pi_ai::models::{create_provider, CreateProviderOptions, Models, Provider, ProviderApiSpec};
use pi_ai::types::ProviderHeaders;

use crate::core::model_config::{ModelConfig, ModelsJsonProvider};
use crate::core::provider_composer::{
    apply_model_overrides, apply_models_json, config_value_env_var_names, is_command_config_value,
    with_composed_models,
};
use crate::core::resolve_config_value::resolve_config_value;

/// A view of one provider's configured auth (upstream `AuthStatus`).
pub use crate::core::provider_composer::AuthStatus;

/// Model registry facade over a pi-ai `Models` collection plus a `ModelConfig`
/// overlay.
///
/// The merged model list is recomputed lazily from the base catalog whenever
/// requested, mirroring the upstream behavior where `getModels()` re-applies
/// models.json on every call (the upstream caches; recomputation here keeps
/// the surface correct with much simpler state management).
#[derive(Clone)]
pub struct ModelRegistry {
    models: Models,
    config: Arc<ModelConfig>,
}

/// API-key auth composed from a bundled provider and models.json overrides.
/// A stored/runtime credential wins. Without one, a configured literal,
/// environment template, or command-backed key is resolved at request time.
/// Configured headers and `authHeader` are applied to both inherited and
/// configured-key results, matching upstream `composeApiKeyAuth`.
struct ConfiguredApiKeyAuth {
    name: String,
    inherited: Option<Arc<dyn ApiKeyAuth>>,
    raw_key: Option<String>,
    raw_headers: Option<BTreeMap<String, String>>,
    auth_header: bool,
}

impl ConfiguredApiKeyAuth {
    fn resolve_value(raw: &str, ctx: &AuthContext) -> Option<String> {
        let env = config_value_env_var_names(raw)
            .into_iter()
            .map(|name| ctx.env(&name).map(|value| (name, value)))
            .collect::<Option<HashMap<_, _>>>()?;
        resolve_config_value(raw, (!env.is_empty()).then_some(&env))
            .filter(|value| !value.trim().is_empty())
    }

    fn resolve_headers(&self, ctx: &AuthContext) -> Option<Option<ProviderHeaders>> {
        let Some(raw_headers) = &self.raw_headers else {
            return Some(None);
        };
        let mut headers = ProviderHeaders::new();
        for (name, raw) in raw_headers {
            headers.insert(name.clone(), Some(Self::resolve_value(raw, ctx)?));
        }
        Some((!headers.is_empty()).then_some(headers))
    }

    fn with_configured_headers(
        &self,
        mut result: AuthResult,
        ctx: &AuthContext,
    ) -> Option<AuthResult> {
        let configured = self.resolve_headers(ctx)?;
        result.auth.headers =
            pi_ai::models::merge_headers(result.auth.headers.as_ref(), configured.as_ref());
        if self.auth_header {
            let key = result.auth.api_key.as_deref()?;
            let mut headers = result.auth.headers.take().unwrap_or_default();
            headers.insert("Authorization".to_string(), Some(format!("Bearer {key}")));
            result.auth.headers = Some(headers);
        }
        Some(result)
    }
}

fn oauth_credential_auth_context(credential: &OAuthCredential) -> AuthContext {
    let ambient = AuthContext::default();
    let extra = credential.extra.clone();
    let ambient_env = ambient.env.clone();
    AuthContext {
        env: Arc::new(move |name| {
            extra
                .get(name)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .or_else(|| ambient_env(name))
        }),
        file_exists: ambient.file_exists,
    }
}

impl ApiKeyAuth for ConfiguredApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login(&self, interaction: &dyn AuthInteraction) -> Result<ApiKeyCredential, PiAiError> {
        if let Some(inherited) = &self.inherited {
            return inherited.login(interaction);
        }
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err(PiAiError::LoginCancelled);
        }
        let key = interaction.prompt(&pi_ai::auth::AuthPrompt::Secret {
            message: format!("Enter {}", self.name),
            placeholder: None,
        })?;
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err(PiAiError::LoginCancelled);
        }
        if key.trim().is_empty() {
            return Err(PiAiError::invalid_response("API key cannot be empty"));
        }
        Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        })
    }

    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        if let Some(credential) = credential {
            if let Some(inherited) = &self.inherited {
                return inherited.check(ctx, Some(credential)).or_else(|| {
                    inherited
                        .resolve(ctx, Some(credential))
                        .map(|result| AuthCheck {
                            source: result.source,
                            auth_type: "api_key",
                        })
                });
            }
            if credential
                .key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
            {
                return Some(AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            return None;
        }
        if let Some(raw_key) = self.raw_key.as_deref() {
            if !raw_key.trim().is_empty()
                && (is_command_config_value(raw_key)
                    || config_value_env_var_names(raw_key)
                        .into_iter()
                        .all(|name| ctx.env(&name).is_some()))
            {
                return Some(AuthCheck {
                    source: Some("configured API key".to_string()),
                    auth_type: "api_key",
                });
            }
            return None;
        }
        self.inherited.as_ref().and_then(|inherited| {
            inherited.check(ctx, None).or_else(|| {
                inherited.resolve(ctx, None).map(|result| AuthCheck {
                    source: result.source,
                    auth_type: "api_key",
                })
            })
        })
    }

    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult> {
        let result = if let Some(credential) = credential {
            if let Some(inherited) = &self.inherited {
                inherited.resolve(ctx, Some(credential))?
            } else {
                let key = credential
                    .key
                    .as_ref()
                    .filter(|key| !key.trim().is_empty())?
                    .clone();
                AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    env: credential.env.clone(),
                    source: Some("stored credential".to_string()),
                }
            }
        } else if let Some(raw_key) = self.raw_key.as_deref() {
            let key = Self::resolve_value(raw_key, ctx)?;
            if let Some(inherited) = &self.inherited {
                inherited.resolve(
                    ctx,
                    Some(&ApiKeyCredential {
                        key: Some(key),
                        env: None,
                    }),
                )?
            } else {
                AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    env: None,
                    source: Some("configured API key".to_string()),
                }
            }
        } else {
            self.inherited.as_ref()?.resolve(ctx, None)?
        };
        self.with_configured_headers(result, ctx)
    }
}

impl ModelRegistry {
    pub fn new(models: Models, config: ModelConfig) -> Self {
        // Validate eagerly: a broken provider config must surface immediately.
        for provider_id in models
            .get_providers()
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>()
        {
            if let Some(config) = config.get_provider(&provider_id) {
                let base: Vec<Model> = models.get_models(Some(&provider_id));
                if let Err(message) = apply_models_json(&provider_id, &base, Some(config)) {
                    tracing::warn!(
                        provider = %provider_id,
                        error = %message,
                        "models.json provider composition error"
                    );
                }
            }
        }
        Self {
            models,
            config: Arc::new(config),
        }
    }

    /// Recompose this registry against a freshly loaded models.json snapshot
    /// while retaining the immutable base provider set and shared credential
    /// stores. This is the Rust equivalent of `ModelRuntime.refresh()`: a
    /// deleted provider cannot leak from the previously composed facade, and
    /// runtime/stored credentials remain visible to the replacement facade.
    pub fn with_config(&self, config: ModelConfig) -> Self {
        Self::new(self.models.clone(), config)
    }

    /// Get the underlying ModelConfig (its error, if any).
    pub fn get_error(&self) -> Option<&str> {
        self.config.get_error()
    }

    /// All models for one provider after the models.json overlay.
    pub fn get_merged_models(&self, provider_id: &str) -> Vec<Model> {
        let base: Vec<Model> = self.models.get_models(Some(provider_id));
        match apply_models_json(provider_id, &base, self.config.get_provider(provider_id)) {
            Ok(models) => apply_model_overrides(models, self.config.get_provider(provider_id)),
            Err(_) => base,
        }
    }

    /// All models across providers, after the overlay (upstream `getAll`).
    pub fn get_all(&self) -> Vec<Model> {
        let mut merged = Vec::new();
        let mut seen = BTreeSet::new();
        for provider in self.models.get_providers() {
            seen.insert(provider.id.clone());
            merged.extend(self.get_merged_models(&provider.id));
        }
        // models.json may define a provider that is not present in the
        // built-in/native facade. Keep those catalog entries visible to the
        // resolver and list surface in config order; dispatch/auth is still
        // handled by the runtime composition boundary.
        for provider_id in self.config.get_provider_ids() {
            if seen.insert(provider_id.to_owned()) {
                merged.extend(self.get_merged_models(provider_id));
            }
        }
        merged
    }

    /// Auth-gated available models (upstream `getAvailable`). Preserve the
    /// provider's filter hook (for example Copilot's account-owned model
    /// picker) while retaining models.json-only additions for an authenticated
    /// provider.
    pub fn get_available(&self) -> Vec<Model> {
        self.into_models().get_available(None)
    }

    /// Find a model by provider and model id (upstream `find`).
    pub fn find(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.get_merged_models(provider)
            .into_iter()
            .find(|m| m.id == model_id)
    }

    /// Whether the provider has configured auth (upstream
    /// `hasConfiguredAuth`). The pi-ai facade applies env-key auth, so any
    /// provider is considered configured when it can stream; providers with
    /// no auth are unavailable.
    pub fn has_configured_auth(&self, provider: &str) -> bool {
        self.into_models().check_auth(provider).is_some()
    }

    /// Build a `Models` facade whose providers carry the merged catalog
    /// (bundled + models.json overlay). Used by the run path so model
    /// resolution sees user overrides (upstream `applyModelsJson` wiring).
    pub fn into_models(&self) -> pi_ai::models::Models {
        // Isolate the provider map while retaining credential/auth context and
        // the shared ModelsStore. A normal `Models::clone()` shares provider
        // maps and would make repeated composition wrap/mutate the base.
        let merged = self.models.fork_registry();
        let base_providers = self
            .models
            .get_providers()
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect::<BTreeMap<_, _>>();
        let mut provider_ids = base_providers.keys().cloned().collect::<BTreeSet<_>>();
        provider_ids.extend(self.config.get_provider_ids().map(str::to_owned));
        for provider_id in provider_ids {
            let base = base_providers.get(&provider_id);
            let config = self.config.get_provider(&provider_id);
            let models = self.get_merged_models(&provider_id);
            if let Some(provider) =
                self.compose_runtime_provider(&provider_id, base, config, models)
            {
                merged.set_provider(provider);
            }
        }
        merged
    }

    fn compose_runtime_provider(
        &self,
        provider_id: &str,
        base: Option<&Provider>,
        config: Option<&ModelsJsonProvider>,
        models: Vec<Model>,
    ) -> Option<Provider> {
        let inherited_api_key = base.and_then(|provider| provider.auth.api_key.clone());
        let inherited_oauth = base.and_then(|provider| provider.auth.oauth.clone());
        let raw_headers = config.and_then(|config| config.headers.clone());
        let auth_header = config
            .and_then(|config| config.auth_header)
            .unwrap_or(false);
        let should_compose_api_key = inherited_api_key.is_some()
            || config.and_then(|config| config.api_key.as_ref()).is_some()
            || inherited_oauth.is_none();
        let api_key = should_compose_api_key.then(|| {
            Arc::new(ConfiguredApiKeyAuth {
                name: inherited_api_key
                    .as_ref()
                    .map(|auth| auth.name().to_string())
                    .unwrap_or_else(|| "API key".to_string()),
                inherited: inherited_api_key,
                raw_key: config.and_then(|config| config.api_key.clone()),
                raw_headers: raw_headers.clone(),
                auth_header,
            }) as Arc<dyn ApiKeyAuth>
        });
        let oauth = inherited_oauth.map(|inherited| {
            if raw_headers.is_none() && !auth_header {
                return inherited;
            }
            let raw_headers = raw_headers.clone();
            decorate_oauth_auth(
                inherited,
                Arc::new(move |credential, auth| {
                    let helper = ConfiguredApiKeyAuth {
                        name: "API key".to_string(),
                        inherited: None,
                        raw_key: None,
                        raw_headers: raw_headers.clone(),
                        auth_header,
                    };
                    helper
                        .with_configured_headers(
                            AuthResult {
                                auth,
                                env: None,
                                source: None,
                            },
                            &oauth_credential_auth_context(credential),
                        )
                        .map(|result| result.auth)
                }),
            )
        });
        if api_key.is_none() && oauth.is_none() {
            tracing::warn!(
                provider = provider_id,
                "models.json provider has no authentication method configured"
            );
            return None;
        }

        let mut streams = BTreeMap::new();
        let mut preserve_single_streams = false;
        if let Some(base) = base {
            streams.extend(base.streams.clone());
            if let Some(single) = &base.single_streams {
                let base_apis = base
                    .get_models()
                    .into_iter()
                    .map(|model| model.api)
                    .collect::<BTreeSet<_>>();
                preserve_single_streams = models.iter().all(|model| base_apis.contains(&model.api));
                for api in base_apis {
                    streams.entry(api).or_insert_with(|| single.clone());
                }
            }
        }
        for api in models.iter().map(|model| model.api.as_str()) {
            if !streams.contains_key(api) {
                if let Some(provider_streams) = pi_ai::providers::provider_streams_for_api(api) {
                    streams.insert(api.to_string(), provider_streams);
                }
            }
        }

        if let Some(base) = base {
            let mut provider = with_composed_models(base, models);
            provider.name = config
                .and_then(|config| config.name.clone())
                .unwrap_or_else(|| base.name.clone());
            provider.base_url = config
                .and_then(|config| config.base_url.clone())
                .or_else(|| base.base_url.clone());
            provider.auth = ProviderAuth { api_key, oauth };
            if !preserve_single_streams {
                provider.single_streams = None;
                provider.streams = streams;
            }
            return Some(provider);
        }

        Some(create_provider(CreateProviderOptions {
            id: provider_id.to_string(),
            name: Some(
                config
                    .and_then(|config| config.name.clone())
                    .unwrap_or_else(|| provider_id.to_string()),
            ),
            base_url: config.and_then(|config| config.base_url.clone()),
            headers: None,
            auth: ProviderAuth { api_key, oauth },
            models,
            api: ProviderApiSpec::ByApi(streams),
            filter_models: None,
        }))
    }

    /// Register/replace a provider at runtime (upstream `registerProvider`
    /// native-provider path).
    pub fn register_provider(&self, provider: pi_ai::models::Provider) {
        self.models.set_provider(provider);
    }

    /// Remove a provider (upstream `unregisterProvider`).
    pub fn unregister_provider(&self, provider: &str) {
        self.models.delete_provider(provider);
    }

    /// Get a provider ('upstream `getProvider`).
    pub fn get_provider(&self, provider: &str) -> Option<pi_ai::models::Provider> {
        self.models.get_provider(provider)
    }

    /// Provider display name (upstream `getProviderDisplayName`). The
    /// composed provider name follows composeModelProvider priority:
    /// extension name -> models.json config name -> builtin name -> id.
    pub fn get_provider_display_name(&self, provider: &str) -> String {
        self.config
            .get_provider(provider)
            .and_then(|c| c.name.clone())
            .or_else(|| self.models.get_provider(provider).map(|p| p.name))
            .unwrap_or_else(|| provider.to_string())
    }

    /// Registered provider ids, including models.json-only providers
    /// (upstream `getRegisteredProviderIds`).
    pub fn get_registered_provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .models
            .get_providers()
            .iter()
            .map(|p| p.id.clone())
            .collect();
        for provider_id in self.config.get_provider_ids() {
            if !ids.iter().any(|id| id == provider_id) {
                ids.push(provider_id.to_string());
            }
        }
        ids.sort();
        ids
    }

    /// Access the underlying pi-ai Models facade.
    pub fn models_facade(&self) -> &Models {
        &self.models
    }
}

/// Construct the coding-agent model facade with the persisted dynamic
/// provider catalog overlay enabled. The low-level pi-ai registry remains
/// credential/source agnostic; this helper supplies the coding-agent store
/// that backs `models-store.json`.
pub fn builtin_models() -> Models {
    let store = Arc::new(crate::core::models_store::FileModelsStore::new(
        crate::core::models_store::FileModelsStore::default_path(),
    ));
    let credentials = Arc::new(crate::core::auth_storage::FileCredentialStore::new(
        crate::config::get_auth_path(),
    ));
    pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(store),
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::auth::{Credential, CredentialStore, InMemoryCredentialStore, OAuthCredential};
    use serde_json::json;

    #[test]
    fn merged_models_include_catalog_and_overlay() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "google": {
                    "baseUrl": "https://overridden.example.com/v1",
                    "models": [
                        { "id": "custom-gemini", "name": "Custom Gemini", "api": "google", "reasoning": true,
                          "cost": { "input": 0.5, "output": 1.5, "cacheRead": 0.1, "cacheWrite": 1.0 },
                          "contextWindow": 200000, "maxTokens": 8192 }
                    ]
                }
            }
        })).unwrap();
        let registry = ModelRegistry::new(models, config);
        let google_models = registry.get_merged_models("google");
        assert!(
            google_models.len() >= 2,
            "overlay must be upserted onto catalog"
        );
        let custom = google_models
            .iter()
            .find(|m| m.id == "custom-gemini")
            .unwrap();
        assert_eq!(custom.provider, "google");
        assert_eq!(custom.base_url, "https://overridden.example.com/v1");
        assert!(custom.reasoning);
        // Catalog models get remapped to the overlay base url.
        let catalog = google_models
            .iter()
            .find(|m| m.id == "gemini-3.1-pro-preview")
            .unwrap();
        assert_eq!(catalog.base_url, "https://overridden.example.com/v1");
    }

    #[test]
    fn registered_provider_ids_include_models_json_only() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": { "custom-provider": { "baseUrl": "https://x", "api": "openai-responses", "models": [] } }
        })).unwrap();
        let registry = ModelRegistry::new(models, config);
        let ids = registry.get_registered_provider_ids();
        assert!(ids.iter().any(|id| id == "custom-provider"));
        assert!(ids.iter().any(|id| id == "google"));
    }

    #[test]
    fn find_resolves_overlay_model() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "google": { "models": [
                    { "id": "overlay-model", "api": "google", "reasoning": false,
                      "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 } }
                ] }
            }
        }))
        .unwrap();
        let registry = ModelRegistry::new(models, config);
        assert!(registry.find("google", "overlay-model").is_some());
        assert!(registry.find("google", "does-not-exist").is_none());
    }

    #[test]
    fn display_name_prefers_models_json() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": { "google": { "name": "Google Overlay" } }
        }))
        .unwrap();
        let registry = ModelRegistry::new(models, config);
        assert_eq!(
            registry.get_provider_display_name("google"),
            "Google Overlay"
        );
    }

    #[test]
    fn unregister_removes_provider() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let registry = ModelRegistry::new(models, ModelConfig::default());
        assert!(registry.get_provider("google").is_some());
        registry.unregister_provider("google");
        assert!(registry.get_provider("google").is_none());
    }

    #[test]
    fn available_models_require_real_provider_auth() {
        let auth_context = pi_ai::auth::AuthContext {
            env: Arc::new(|_| None),
            file_exists: Arc::new(|_| false),
        };
        let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions {
            auth_context: Some(auth_context),
            ..Default::default()
        });
        let registry = ModelRegistry::new(models, ModelConfig::default());

        assert!(registry.get_available().is_empty());
        assert!(!registry.has_configured_auth("google"));
        assert!(!registry.has_configured_auth("not-a-provider"));
    }

    #[test]
    fn get_all_preserves_runtime_provider_and_catalog_order() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let expected = models
            .get_models(None)
            .into_iter()
            .map(|model| (model.provider, model.id))
            .collect::<Vec<_>>();
        let registry = ModelRegistry::new(models, ModelConfig::default());
        let actual = registry
            .get_all()
            .into_iter()
            .map(|model| (model.provider, model.id))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn get_all_includes_models_json_only_provider() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "custom-provider": {
                    "baseUrl": "https://custom.example/v1",
                    "api": "openai-responses",
                    "models": [
                        { "id": "custom-model", "name": "Custom model" }
                    ]
                }
            }
        }))
        .unwrap();
        let registry = ModelRegistry::new(models, config);

        let custom = registry
            .get_all()
            .into_iter()
            .find(|model| model.provider == "custom-provider")
            .expect("models.json-only provider must be visible in get_all");
        assert_eq!(custom.id, "custom-model");
        assert_eq!(custom.base_url, "https://custom.example/v1");
        assert_eq!(custom.api, "openai-responses");
    }

    #[test]
    fn available_models_apply_provider_specific_filtering() {
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("availableModelIds".to_string(), json!(["gpt-4.1"]));
        credentials.modify("github-copilot", &|_| {
            Some(Credential::OAuth(OAuthCredential {
                refresh: "refresh".to_string(),
                access: "access".to_string(),
                expires: u64::MAX,
                extra: extra.clone(),
            }))
        });
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions {
            credentials: Some(credentials),
            auth_context: Some(pi_ai::auth::AuthContext {
                env: Arc::new(|_| None),
                file_exists: Arc::new(|_| false),
            }),
            ..Default::default()
        });
        models.set_provider(pi_ai::providers::github_copilot_provider());
        let registry = ModelRegistry::new(models, ModelConfig::default());

        let ids = registry
            .get_available()
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-4.1"]);
    }

    #[test]
    fn into_models_registers_models_json_only_provider_with_configured_auth() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "custom-runtime": {
                    "name": "Custom Runtime",
                    "baseUrl": "https://custom.example/v1",
                    "api": "openai-completions",
                    "apiKey": "synthetic-models-json-key",
                    "authHeader": true,
                    "headers": { "X-Custom-Header": "custom-value" },
                    "models": [{ "id": "custom-model", "name": "Custom model" }]
                }
            }
        }))
        .unwrap();
        let registry = ModelRegistry::new(models, config);
        let facade = registry.into_models();

        assert!(
            registry
                .models_facade()
                .get_provider("custom-runtime")
                .is_none(),
            "composition must not mutate the source provider registry"
        );

        let provider = facade
            .get_provider("custom-runtime")
            .expect("models.json-only provider must be registered");
        assert_eq!(provider.name, "Custom Runtime");
        assert!(provider.streams.contains_key("openai-completions"));
        let model = facade
            .get_model("custom-runtime", "custom-model")
            .expect("custom model must be dispatchable");
        let auth = facade
            .get_auth("custom-runtime", Some(&model))
            .expect("configured models.json key must authenticate");
        assert_eq!(
            auth.auth.api_key.as_deref(),
            Some("synthetic-models-json-key")
        );
        let headers = auth.auth.headers.expect("configured headers");
        assert_eq!(
            headers
                .get("Authorization")
                .and_then(|value| value.as_deref()),
            Some("Bearer synthetic-models-json-key")
        );
        assert_eq!(
            headers
                .get("X-Custom-Header")
                .and_then(|value| value.as_deref()),
            Some("custom-value")
        );
        assert!(registry.has_configured_auth("custom-runtime"));
        assert!(registry
            .get_available()
            .iter()
            .any(|available| available.provider == "custom-runtime"
                && available.id == "custom-model"));
        assert!(
            registry
                .into_models()
                .get_auth("custom-runtime", Some(&model))
                .is_some(),
            "repeated composition remains idempotent"
        );
    }

    #[tokio::test]
    async fn unsupported_models_json_api_remains_a_deterministic_dispatch_error() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "custom-unsupported": {
                    "baseUrl": "https://custom.example/v1",
                    "api": "unknown-api",
                    "apiKey": "synthetic-key",
                    "models": [{ "id": "unknown-model" }]
                }
            }
        }))
        .unwrap();
        let facade = ModelRegistry::new(models, config).into_models();
        let provider = facade
            .get_provider("custom-unsupported")
            .expect("catalog remains registered for deterministic failure");
        assert!(provider.streams.is_empty());
        assert!(provider.single_streams.is_none());
        let model = facade
            .get_model("custom-unsupported", "unknown-model")
            .expect("unsupported model remains addressable");
        let context = pi_ai::types::Context {
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let (_events, message) = provider.stream(&model, &context, None).collect().await;
        assert_eq!(message.stop_reason(), Some(pi_ai::types::StopReason::Error));
        assert_eq!(
            message.error_message(),
            Some("Provider custom-unsupported has no API implementation for \"unknown-api\"")
        );
    }

    #[test]
    fn models_json_auth_resolves_provider_scoped_environment_templates() {
        let auth_context = AuthContext {
            env: Arc::new(|name| match name {
                "CUSTOM_RUNTIME_KEY" => Some("scoped-key".to_string()),
                "CUSTOM_RUNTIME_HEADER" => Some("scoped-header".to_string()),
                _ => None,
            }),
            file_exists: Arc::new(|_| false),
        };
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions {
            auth_context: Some(auth_context),
            ..Default::default()
        });
        let config = ModelConfig::from_value(json!({
            "providers": {
                "custom-env": {
                    "baseUrl": "https://custom.example/v1",
                    "api": "openai-completions",
                    "apiKey": "$CUSTOM_RUNTIME_KEY",
                    "headers": { "X-Scoped": "${CUSTOM_RUNTIME_HEADER}" },
                    "models": [{ "id": "custom-env-model" }]
                }
            }
        }))
        .unwrap();
        let facade = ModelRegistry::new(models, config).into_models();
        let model = facade
            .get_model("custom-env", "custom-env-model")
            .expect("environment-backed model");
        let auth = facade
            .get_auth("custom-env", Some(&model))
            .expect("environment templates resolve through AuthContext");

        assert_eq!(auth.auth.api_key.as_deref(), Some("scoped-key"));
        assert_eq!(
            auth.auth
                .headers
                .as_ref()
                .and_then(|headers| headers.get("X-Scoped").and_then(|value| value.as_deref())),
            Some("scoped-header")
        );
    }

    #[test]
    fn models_json_headers_decorate_inherited_oauth_without_api_key_login() {
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let mut extra = BTreeMap::new();
        extra.insert("OAUTH_SCOPED_HEADER".to_string(), json!("oauth-header"));
        credentials.modify("openai-codex", &|_| {
            Some(Credential::OAuth(OAuthCredential {
                refresh: "synthetic-refresh".to_string(),
                access: "synthetic-access".to_string(),
                expires: u64::MAX,
                extra: extra.clone(),
            }))
        });
        let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions {
            credentials: Some(credentials),
            ..Default::default()
        });
        let config = ModelConfig::from_value(json!({
            "providers": {
                "openai-codex": {
                    "headers": { "X-OAuth-Scoped": "$OAUTH_SCOPED_HEADER" },
                    "authHeader": true
                }
            }
        }))
        .unwrap();
        let facade = ModelRegistry::new(models, config).into_models();
        let provider = facade
            .get_provider("openai-codex")
            .expect("composed Codex provider");
        assert!(provider.auth.api_key.is_none());
        assert!(provider.auth.oauth.is_some());
        let model = facade
            .get_models(Some("openai-codex"))
            .into_iter()
            .next()
            .expect("Codex model");
        let auth = facade
            .get_auth("openai-codex", Some(&model))
            .expect("stored OAuth auth");
        assert_eq!(auth.auth.api_key.as_deref(), Some("synthetic-access"));
        let headers = auth.auth.headers.expect("decorated OAuth headers");
        assert_eq!(
            headers
                .get("Authorization")
                .and_then(|value| value.as_deref()),
            Some("Bearer synthetic-access")
        );
        assert_eq!(
            headers
                .get("X-OAuth-Scoped")
                .and_then(|value| value.as_deref()),
            Some("oauth-header")
        );
    }
}

#[cfg(test)]
mod run_path_merge_tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn into_models_carries_overlay_into_facade() {
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let config = ModelConfig::from_value(json!({
            "providers": {
                "google": {
                    "baseUrl": "https://overridden.example.com/v1",
                    "models": [
                        { "id": "custom-gemini", "name": "Custom Gemini", "api": "google", "reasoning": true,
                          "cost": { "input": 0.5, "output": 1.5, "cacheRead": 0.1, "cacheWrite": 1.0 },
                          "contextWindow": 200000, "maxTokens": 8192 }
                    ]
                }
            }
        })).unwrap();
        let registry = ModelRegistry::new(models, config);
        let facade = registry.into_models();
        // The facade resolves the overridden model.
        let custom = facade
            .get_model("google", "custom-gemini")
            .expect("overlay model in facade");
        assert_eq!(custom.base_url, "https://overridden.example.com/v1");
        // Catalog models are remapped to the overlay base url too.
        let catalog = facade
            .get_model("google", "gemini-3.1-pro-preview")
            .expect("catalog model in facade");
        assert_eq!(catalog.base_url, "https://overridden.example.com/v1");
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn into_models_preserves_store_and_provider_capabilities() {
        let store = std::sync::Arc::new(pi_ai::models::InMemoryModelsStore::new());
        let store_trait: std::sync::Arc<dyn pi_ai::models::ModelsStore> = store.clone();
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions {
            models_store: Some(store_trait.clone()),
            ..Default::default()
        });
        let _core = crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        let registry = ModelRegistry::new(models.clone(), ModelConfig::default());
        let facade = registry.into_models();
        assert!(std::sync::Arc::ptr_eq(&store_trait, &facade.models_store()));
        let provider = facade.get_provider("faux").expect("composed faux provider");
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
    }

    #[test]
    fn models_json_path_returns_none_when_missing() {
        // No agent dir models.json in the test environment.
        let path = crate::core::model_config::models_json_path();
        // Either None or a real file; assert consistency with existence.
        if let Some(p) = &path {
            assert!(p.exists(), "reported path must exist: {p:?}");
        }
    }
}
