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

use std::collections::BTreeSet;
use std::sync::Arc;

use pi_ai::model::Model;
use pi_ai::models::Models;

use crate::core::model_config::ModelConfig;
use crate::core::provider_composer::{
    apply_model_overrides, apply_models_json, with_composed_models,
};

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
        let available_base = self
            .models
            .get_available(None)
            .into_iter()
            .map(|model| (model.provider, model.id))
            .collect::<BTreeSet<_>>();
        let base_ids = self
            .models
            .get_models(None)
            .into_iter()
            .map(|model| (model.provider, model.id))
            .collect::<BTreeSet<_>>();

        self.get_all()
            .into_iter()
            .filter(|model| {
                let key = (model.provider.clone(), model.id.clone());
                if base_ids.contains(&key) {
                    available_base.contains(&key)
                } else {
                    self.models.check_auth(&model.provider).is_some()
                }
            })
            .collect()
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
        self.models.check_auth(provider).is_some()
    }

    /// Build a `Models` facade whose providers carry the merged catalog
    /// (bundled + models.json overlay). Used by the run path so model
    /// resolution sees user overrides (upstream `applyModelsJson` wiring).
    pub fn into_models(&self) -> pi_ai::models::Models {
        // Clone the facade instead of constructing a fresh one: this retains
        // credential/auth context and the shared ModelsStore used by remote
        // catalogs while replacing only each provider's composed model list.
        let merged = self.models.clone();
        for provider in self.models.get_providers() {
            merged.set_provider(with_composed_models(
                &provider,
                self.get_merged_models(&provider.id),
            ));
        }
        merged
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
