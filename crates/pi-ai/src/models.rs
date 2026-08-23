//! Models facade — port of `packages/ai/src/models.ts` + `models-store.ts`.
//!
//! `Provider` is the concrete runtime unit (id/name/base metadata, auth
//! methods, model listing, stream behavior). `Models` is the collection of
//! providers with auth application and stream convenience. `createModels`
//! builds a `Models`; `createProvider` builds a `Provider` from parts.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth::{
    AuthCheck, AuthContext, AuthResult, Credential, CredentialStore, InMemoryCredentialStore,
    ProviderAuth,
};
use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::Model;
use crate::types::{
    AssistantMessage, Context, DeferredHandle, ProviderHeaders, ProviderRequestOptions,
    SimpleStreamOptions, StreamOptions,
};

/// Error codes for the Models facade (upstream `ModelsError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsErrorCode {
    Provider,
    Auth,
    Stream,
    Oauth,
    ModelSource,
    UnknownProvider,
}

#[derive(Debug, Clone)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(code: ModelsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ModelsError {}

/// Full stream function: `(model, context, options) -> event stream`.
/// Auth is applied by the `Models` facade before dispatch.
pub type StreamFn = Arc<
    dyn Fn(&Model, &Context, Option<&StreamOptions>) -> AssistantMessageEventStream + Send + Sync,
>;

/// Simple (provider-neutral) stream function.
pub type SimpleStreamFn = Arc<
    dyn Fn(&Model, &Context, Option<&SimpleStreamOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

/// Provider-scoped availability filter (upstream `filterModels`).
pub type FilterModelsFn = Arc<dyn Fn(&[Model], Option<&Credential>) -> Vec<Model> + Send + Sync>;

/// ModelsStore entry — persistent model catalogs keyed by provider ID.
#[derive(Debug, Clone, Default)]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    pub last_modified: Option<u64>,
    pub checked_at: Option<u64>,
    pub etag: Option<String>,
}

/// Persistent model catalogs keyed by provider ID (upstream `ModelsStore`).
pub trait ModelsStore: Send + Sync {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry>;
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry);
    fn delete(&self, provider_id: &str);
}

/// In-memory models store (upstream `InMemoryModelsStore`).
#[derive(Default, Clone)]
pub struct InMemoryModelsStore {
    entries: Arc<std::sync::Mutex<BTreeMap<String, ModelsStoreEntry>>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ModelsStore for InMemoryModelsStore {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry> {
        self.entries.lock().unwrap().get(provider_id).cloned()
    }
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        self.entries
            .lock()
            .unwrap()
            .insert(provider_id.to_string(), entry.clone());
    }
    fn delete(&self, provider_id: &str) {
        self.entries.lock().unwrap().remove(provider_id);
    }
}

/// Deferred-response fetch function: `(model, handle, options) -> stream`
/// (upstream `fetchDeferred`). `None` means the provider does not support
/// deferred responses.
pub type DeferredStreamFn = Arc<
    dyn Fn(&Model, &DeferredHandle, &DeferredFetchOptions) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

/// Cancellation for a deferred handle: `(model, handle, options)`.
pub type DeferredCancelFn =
    Arc<dyn Fn(&Model, &DeferredHandle, &DeferredFetchOptions) -> Result<(), String> + Send + Sync>;

/// Options for deferred fetch/cancel (upstream `DeferredFetchOptions`).
#[derive(Clone, Default)]
pub struct DeferredFetchOptions {
    pub base: crate::types::ProviderRequestOptions,
    pub cancel_after_ms: Option<u64>,
}

#[derive(Clone)]
pub struct ProviderStreams {
    pub stream: StreamFn,
    pub stream_simple: SimpleStreamFn,
    /// Optional deferred-response resolution for providers that support it.
    pub fetch_deferred: Option<DeferredStreamFn>,
    pub cancel_deferred: Option<DeferredCancelFn>,
}

/// A provider is the concrete runtime unit. It owns id/name/base metadata,
/// auth methods, model listing, and stream behavior.
#[derive(Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub auth: ProviderAuth,
    /// Static baseline models (createProvider merges dynamic overlay).
    pub models: Vec<Model>,
    /// Stream dispatcher by model api. Key is `model.api`; missing entries
    /// produce a stream error (upstream apiFor dispatch).
    pub streams: BTreeMap<String, ProviderStreams>,
    /// Optional primary stream/simple pair when all models share one api.
    pub single_streams: Option<ProviderStreams>,
    pub filter_models: Option<FilterModelsFn>,
}

impl Provider {
    /// Current known models, sync (mirrors upstream `currentModels`).
    pub fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    /// Stream dispatcher: single implementation wins; otherwise dispatch on
    /// model.api; missing produces the upstream stream error.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(s) => (s.stream)(model, context, options),
            None => make_unknown_api_error_stream(model, &self.id),
        }
    }

    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(s) => (s.stream_simple)(model, context, options),
            None => make_unknown_api_error_stream(model, &self.id),
        }
    }

    fn api_for(&self, model: &Model) -> Option<&ProviderStreams> {
        if let Some(single) = &self.single_streams {
            return Some(single);
        }
        self.streams.get(&model.api)
    }
}

fn error_message_for(model: &Model, message: &str) -> AssistantMessage {
    let mut msg = AssistantMessage::new();
    msg.set_api_provider_model(&model.api, &model.provider, &model.id);
    msg.set_stop_reason(crate::types::StopReason::Error);
    let AssistantMessage::Assistant { error_message, .. } = &mut msg;
    *error_message = Some(message.to_string());
    msg
}

fn make_unknown_api_error_stream(model: &Model, provider_id: &str) -> AssistantMessageEventStream {
    let message = format!(
        "Provider {provider_id} has no API implementation for \"{}\"",
        model.api
    );
    crate::event_stream::create_error_stream(&model.api, provider_id, &model.id, message)
}

/// createProvider input (upstream `CreateProviderOptions`).
pub struct CreateProviderOptions {
    pub id: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub auth: ProviderAuth,
    pub models: Vec<Model>,
    pub api: ProviderApiSpec,
    pub filter_models: Option<FilterModelsFn>,
}

/// Provider API specification: single implementation for all models, or a map
/// keyed by model.api for mixed-API providers.
pub enum ProviderApiSpec {
    Single(ProviderStreams),
    ByApi(BTreeMap<String, ProviderStreams>),
}

/// Builds a provider from parts (upstream `createProvider`).
pub fn create_provider(input: CreateProviderOptions) -> Provider {
    let (single_streams, streams) = match input.api {
        ProviderApiSpec::Single(s) => (Some(s), BTreeMap::new()),
        ProviderApiSpec::ByApi(m) => (None, m),
    };

    Provider {
        id: input.id.clone(),
        name: input.name.unwrap_or_else(|| input.id.clone()),
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        models: input.models,
        streams,
        single_streams,
        filter_models: input.filter_models,
    }
}

/// Merge provider headers with per-field case-insensitive override
/// (upstream `mergeHeaders`).
pub fn merge_headers(
    base: Option<&ProviderHeaders>,
    override_: Option<&ProviderHeaders>,
) -> Option<ProviderHeaders> {
    match (base, override_) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => {
            let mut merged = b.clone();
            for (name, value) in o {
                let lower = name.to_lowercase();
                merged.retain(|k, _| k.to_lowercase() != lower);
                merged.insert(name.clone(), value.clone());
            }
            Some(merged)
        }
    }
}

/// Options for createModels (upstream `CreateModelsOptions`).
#[derive(Default)]
pub struct CreateModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub models_store: Option<Arc<dyn ModelsStore>>,
    pub auth_context: Option<AuthContext>,
}

/// Runtime collection of providers plus auth application and stream
/// convenience. Mirrors upstream `MutableModels`.
#[derive(Clone)]
pub struct Models {
    providers: Arc<std::sync::RwLock<BTreeMap<String, Provider>>>,
    credentials: Arc<dyn CredentialStore>,
    #[allow(dead_code)] // refresh machinery (P8) reads persistence
    models_store: Arc<dyn ModelsStore>,
    auth_context: AuthContext,
}

/// Build a Models collection (upstream `createModels`).
pub fn create_models(options: CreateModelsOptions) -> Models {
    Models {
        providers: Arc::new(std::sync::RwLock::new(BTreeMap::new())),
        credentials: options
            .credentials
            .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
        models_store: options
            .models_store
            .unwrap_or_else(|| Arc::new(InMemoryModelsStore::new())),
        auth_context: options.auth_context.unwrap_or_default(),
    }
}

impl Models {
    pub fn set_provider(&self, provider: Provider) {
        self.providers
            .write()
            .unwrap()
            .insert(provider.id.clone(), provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.providers.write().unwrap().remove(id);
    }

    pub fn clear_providers(&self) {
        self.providers.write().unwrap().clear();
    }

    pub fn get_providers(&self) -> Vec<Provider> {
        self.providers.read().unwrap().values().cloned().collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<Provider> {
        self.providers.read().unwrap().get(id).cloned()
    }

    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self.providers.read().unwrap();
        if let Some(provider_id) = provider {
            match providers.get(provider_id) {
                Some(p) => p.get_models(),
                None => Vec::new(),
            }
        } else {
            let mut all = Vec::new();
            for provider in providers.values() {
                all.extend(provider.get_models());
            }
            all
        }
    }

    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|m| m.id == id)
    }

    /// Check whether a provider has complete auth configuration (upstream
    /// `checkAuth`).
    pub fn check_auth(&self, provider_id: &str) -> Option<AuthCheck> {
        let provider = self.get_provider(provider_id)?;
        let credential = self.credentials.read(provider_id);
        self.check_provider_auth(&provider, credential.as_ref())
    }

    fn check_provider_auth(
        &self,
        provider: &Provider,
        credential: Option<&Credential>,
    ) -> Option<AuthCheck> {
        if let Some(Credential::OAuth(_)) = credential {
            return provider.auth.oauth.as_ref().map(|_| AuthCheck {
                source: Some("OAuth".to_string()),
                auth_type: "oauth",
            });
        }
        let api_key_cred = match credential {
            Some(Credential::ApiKey(c)) => Some(c),
            _ => None,
        };
        let api_key = provider.auth.api_key.as_ref()?;
        if let Some(check) = api_key.check(&self.auth_context, api_key_cred) {
            return Some(check);
        }
        let resolution = self.resolve_provider_auth(provider, api_key_cred);
        resolution.map(|r| AuthCheck {
            source: r.source,
            auth_type: "api_key",
        })
    }

    /// Return models whose providers have complete auth configuration
    /// (upstream `getAvailable`).
    pub fn get_available(&self, provider_id: Option<&str>) -> Vec<Model> {
        let providers: Vec<Provider> = match provider_id {
            Some(id) => self.get_provider(id).map(|p| vec![p]).unwrap_or_default(),
            None => self.get_providers(),
        };
        let mut available = Vec::new();
        for provider in providers {
            let credential = self.credentials.read(&provider.id);
            let auth = self.check_provider_auth(&provider, credential.as_ref());
            if auth.is_none() {
                continue;
            }
            let models = provider.get_models();
            let filtered = match &provider.filter_models {
                Some(f) => f(&models, credential.as_ref()),
                None => models,
            };
            available.extend(filtered);
        }
        available
    }

    /// Resolve provider-scoped auth by provider id, or provider auth plus
    /// static model headers when passed a model (upstream `getAuth`).
    pub fn get_auth(&self, provider_id: &str, model: Option<&Model>) -> Option<AuthResult> {
        let provider = self.get_provider(provider_id)?;
        let credential = self.credentials.read(provider_id);
        // OAuth credentials derive request auth through the provider's OAuth
        // flow (upstream `getAuth` OAuth branch).
        if let Some(Credential::OAuth(oauth_cred)) = credential.as_ref() {
            if let Some(oauth) = provider.auth.oauth.as_ref() {
                if let Some(auth) = oauth.to_auth(oauth_cred) {
                    let mut result = AuthResult {
                        auth,
                        env: None,
                        source: Some("OAuth".to_string()),
                    };
                    if let Some(model) = model {
                        if let Some(headers) = &model.headers {
                            let headers_with_options: ProviderHeaders = headers
                                .iter()
                                .map(|(k, v)| (k.clone(), Some(v.clone())))
                                .collect();
                            let mut auth = result.auth.clone();
                            auth.headers =
                                merge_headers(auth.headers.as_ref(), Some(&headers_with_options));
                            result.auth = auth;
                        }
                    }
                    return Some(result);
                }
            }
        }
        let api_key_cred = match credential.as_ref() {
            Some(Credential::ApiKey(c)) => Some(c),
            _ => None,
        };
        let mut result = self.resolve_provider_auth(&provider, api_key_cred)?;
        // Model-static headers merge (upstream getAuth(model)).
        if let Some(model) = model {
            if let Some(headers) = &model.headers {
                let headers_with_options: ProviderHeaders = headers
                    .iter()
                    .map(|(k, v)| (k.clone(), Some(v.clone())))
                    .collect();
                let mut auth = result.auth.clone();
                auth.headers = merge_headers(auth.headers.as_ref(), Some(&headers_with_options));
                result.auth = auth;
            }
        }
        Some(result)
    }

    fn resolve_provider_auth(
        &self,
        provider: &Provider,
        credential: Option<&crate::auth::ApiKeyCredential>,
    ) -> Option<AuthResult> {
        let api_key = provider.auth.api_key.as_ref()?;
        api_key.resolve(&self.auth_context, credential)
    }

    /// Apply auth for a model request (upstream `applyAuth`): resolves auth,
    /// merges headers, preps request options.
    pub fn apply_auth(
        &self,
        model: &Model,
        options: &ProviderRequestOptions,
    ) -> Result<(Model, ProviderRequestOptions), ModelsError> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::UnknownProvider,
                format!("Unknown provider: {}", model.provider),
            )
        })?;
        let _ = provider; // used only for existence check and header source below
        let resolution = self.get_auth(&model.provider, Some(model)).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Auth,
                format!("Provider is not configured: {}", model.provider),
            )
        })?;

        let api_key = options
            .api_key
            .clone()
            .or_else(|| resolution.auth.api_key.clone());
        let headers = merge_headers(resolution.auth.headers.as_ref(), options.headers.as_ref());
        let env = match (&resolution.env, &options.env) {
            (None, None) => None,
            (Some(r), None) => Some(r.clone()),
            (None, Some(o)) => Some(o.clone()),
            (Some(r), Some(o)) => {
                let mut merged = r.clone();
                for (k, v) in o {
                    merged.insert(k.clone(), v.clone());
                }
                Some(merged)
            }
        };
        let request_model = match &resolution.auth.base_url {
            Some(base_url) => {
                let mut m = model.clone();
                m.base_url = base_url.clone();
                m
            }
            None => model.clone(),
        };
        let request_options = ProviderRequestOptions {
            api_key,
            env,
            headers,
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            telemetry_context: options.telemetry_context.clone(),
        };
        Ok((request_model, request_options))
    }

    /// Stream a model request through its provider with auth applied
    /// (upstream `Models.stream` + `lazyStream`): synchronous return, async
    /// setup; auth failures terminate the stream with an error event.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let models = self.clone();
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned();
        let outer = AssistantMessageEventStream::new();
        let tx = match outer.sender() {
            Some(t) => t,
            None => return outer,
        };
        let error_model = model.clone();
        tokio::spawn(async move {
            let mut pusher = crate::event_stream::StreamSinkAdapter::new(tx.clone());
            let base_options = options.as_ref().map(|o| o.base.clone()).unwrap_or_default();
            let result = models.apply_auth(&model, &base_options);
            match result {
                Ok((request_model, request_options)) => {
                    let stream_options = options.clone().map(|mut o| {
                        o.base = request_options;
                        o
                    });
                    let provider = models.get_provider(&model.provider);
                    match provider {
                        Some(p) => {
                            let inner = p.stream(&request_model, &context, stream_options.as_ref());
                            let final_message = inner
                                .for_each(|event| {
                                    pusher.push(event);
                                })
                                .await;
                            if final_message.stop_reason().is_some() {
                                pusher.end(Some(final_message));
                            } else {
                                pusher.end(None);
                            }
                        }
                        None => {
                            let err = ModelsError::new(
                                ModelsErrorCode::UnknownProvider,
                                format!("Unknown provider: {}", model.provider),
                            );
                            let message = error_message_for(&model, &err.message);
                            pusher.push(crate::types::AssistantMessageEvent::Error {
                                reason: crate::types::ErrorReason::Error,
                                error_message: message.clone(),
                            });
                            pusher.end(Some(message));
                        }
                    }
                }
                Err(err) => {
                    let message = error_message_for(&error_model, &err.message);
                    pusher.push(crate::types::AssistantMessageEvent::Error {
                        reason: crate::types::ErrorReason::Error,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                }
            }
        });
        outer
    }

    /// Convenience: run a stream to completion (upstream `complete`).
    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessage {
        self.stream(model, context, options).for_each(|_| {}).await
    }

    /// Simple (provider-neutral) stream request (upstream `streamSimple`).
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let models = self.clone();
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned();
        let outer = AssistantMessageEventStream::new();
        let tx = match outer.sender() {
            Some(t) => t,
            None => return outer,
        };
        let error_model = model.clone();
        tokio::spawn(async move {
            let mut pusher = crate::event_stream::StreamSinkAdapter::new(tx.clone());
            let base_options = options
                .as_ref()
                .map(|o| o.base.base.clone())
                .unwrap_or_default();
            let result = models.apply_auth(&model, &base_options);
            match result {
                Ok((request_model, request_options)) => {
                    let simple_options = options.clone().map(|mut o| {
                        o.base.base = request_options;
                        o
                    });
                    let provider = models.get_provider(&model.provider);
                    match provider {
                        Some(p) => {
                            let inner =
                                p.stream_simple(&request_model, &context, simple_options.as_ref());
                            let final_message = inner
                                .for_each(|event| {
                                    pusher.push(event);
                                })
                                .await;
                            if final_message.stop_reason().is_some() {
                                pusher.end(Some(final_message));
                            } else {
                                pusher.end(None);
                            }
                        }
                        None => {
                            let err = ModelsError::new(
                                ModelsErrorCode::UnknownProvider,
                                format!("Unknown provider: {}", model.provider),
                            );
                            let message = error_message_for(&model, &err.message);
                            pusher.push(crate::types::AssistantMessageEvent::Error {
                                reason: crate::types::ErrorReason::Error,
                                error_message: message.clone(),
                            });
                            pusher.end(Some(message));
                        }
                    }
                }
                Err(err) => {
                    let message = error_message_for(&error_model, &err.message);
                    pusher.push(crate::types::AssistantMessageEvent::Error {
                        reason: crate::types::ErrorReason::Error,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                }
            }
        });
        outer
    }

    /// Convenience: run a simple stream to completion.
    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options)
            .for_each(|_| {})
            .await
    }

    /// Fetch a previously-deferred response (upstream `Models.fetchDeferred`).
    pub async fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&DeferredFetchOptions>,
    ) -> Result<AssistantMessage, ModelsError> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::UnknownProvider,
                format!("Unknown provider: {}", model.provider),
            )
        })?;
        let streams = provider.api_for(model).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            )
        })?;
        let fetcher = streams.fetch_deferred.clone().ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            )
        })?;
        let base_options = options.map(|o| o.base.clone()).unwrap_or_default();
        let (request_model, request_options) = self.apply_auth(model, &base_options)?;
        let fetch_options = DeferredFetchOptions {
            base: request_options,
            cancel_after_ms: options.and_then(|o| o.cancel_after_ms),
        };
        let stream = fetcher(&request_model, handle, &fetch_options);
        Ok(stream.for_each(|_| {}).await)
    }

    /// Cancel a deferred response (upstream `Models.cancelDeferred`).
    pub async fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&DeferredFetchOptions>,
    ) -> Result<(), ModelsError> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::UnknownProvider,
                format!("Unknown provider: {}", model.provider),
            )
        })?;
        let streams = provider.api_for(model).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            )
        })?;
        let canceller = streams.cancel_deferred.clone().ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!(
                    "Provider {} does not support deferred responses",
                    model.provider
                ),
            )
        })?;
        let base_options = options.map(|o| o.base.clone()).unwrap_or_default();
        let (request_model, request_options) = self.apply_auth(model, &base_options)?;
        let cancel_options = DeferredFetchOptions {
            base: request_options,
            cancel_after_ms: options.and_then(|o| o.cancel_after_ms),
        };
        canceller(&request_model, handle, &cancel_options)
            .map_err(|e| ModelsError::new(ModelsErrorCode::Provider, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthResult, ModelAuth};

    struct TestApiKeyAuth;
    impl ApiKeyAuth for TestApiKeyAuth {
        fn name(&self) -> &str {
            "Test key"
        }
        fn check(
            &self,
            ctx: &AuthContext,
            credential: Option<&ApiKeyCredential>,
        ) -> Option<AuthCheck> {
            if credential.is_some() || ctx.env("TEST_API_KEY").is_some() {
                Some(AuthCheck {
                    source: Some("test".to_string()),
                    auth_type: "api_key",
                })
            } else {
                None
            }
        }
        fn resolve(
            &self,
            ctx: &AuthContext,
            credential: Option<&ApiKeyCredential>,
        ) -> Option<AuthResult> {
            if let Some(cred) = credential {
                if cred.key.is_some() {
                    return Some(AuthResult {
                        auth: ModelAuth {
                            api_key: cred.key.clone(),
                            headers: None,
                            base_url: None,
                        },
                        env: None,
                        source: Some("stored".to_string()),
                    });
                }
            }
            let key = ctx.env("TEST_API_KEY")?;
            Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    headers: None,
                    base_url: None,
                },
                env: None,
                source: Some("TEST_API_KEY".to_string()),
            })
        }
    }

    fn test_provider() -> Provider {
        // Provider whose stream echoes a fixed message.
        let stream = Arc::new(
            |_model: &Model, _ctx: &Context, _options: Option<&StreamOptions>| {
                let mut msg = crate::types::AssistantMessage::new();
                msg.content_mut()
                    .push(crate::types::ContentBlock::text("hello"));
                msg.set_stop_reason(crate::types::StopReason::Stop);
                let stream = AssistantMessageEventStream::new();
                let tx = stream.sender().unwrap();
                tokio::spawn(async move {
                    let mut pusher = crate::event_stream::StreamSinkAdapter::new(tx);
                    pusher.push(crate::types::AssistantMessageEvent::Start {
                        partial: crate::types::AssistantMessage::new(),
                    });
                    pusher.push(crate::types::AssistantMessageEvent::Done {
                        reason: crate::types::DoneReason::Stop,
                        message: msg.clone(),
                    });
                    pusher.end(Some(msg));
                });
                stream
            },
        );
        // A no-op stream_simple (the simple-path tests don't use this provider).
        let stream_simple = Arc::new(
            |_model: &Model, _ctx: &Context, _options: Option<&SimpleStreamOptions>| {
                crate::event_stream::create_error_stream(
                    "test",
                    "test",
                    "m1",
                    "streamSimple not used in tests".to_string(),
                )
            },
        );
        create_provider(CreateProviderOptions {
            id: "test".to_string(),
            name: Some("Test".to_string()),
            base_url: Some("https://test".to_string()),
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(TestApiKeyAuth)),
                oauth: None,
            },
            models: vec![Model::new("m1", "M1", "test-api", "test")],
            api: ProviderApiSpec::Single(ProviderStreams {
                stream,
                stream_simple,
                fetch_deferred: None,
                cancel_deferred: None,
            }),
            filter_models: None,
        })
    }

    #[test]
    fn models_set_get_providers() {
        let models = create_models(CreateModelsOptions::default());
        models.set_provider(test_provider());
        assert_eq!(models.get_providers().len(), 1);
        assert!(models.get_provider("test").is_some());
        assert!(models.get_provider("nope").is_none());
    }

    #[test]
    fn models_get_models_and_model() {
        let models = create_models(CreateModelsOptions::default());
        models.set_provider(test_provider());
        assert_eq!(models.get_models(None).len(), 1);
        assert_eq!(models.get_models(Some("test")).len(), 1);
        assert_eq!(models.get_models(Some("nope")).len(), 0);
        assert!(models.get_model("test", "m1").is_some());
        assert!(models.get_model("test", "nope").is_none());
    }

    fn test_models_with_env(key: Option<&str>) -> Models {
        // AuthContext reading from a closure-owned env map so tests never
        // touch the process environment (parallel-test safe).
        let env_key = key.map(|s| s.to_string());
        let ctx = AuthContext {
            env: Arc::new(move |name: &str| {
                if name == "TEST_API_KEY" {
                    env_key.clone()
                } else {
                    None
                }
            }),
            file_exists: Arc::new(|_| false),
        };
        let models = create_models(CreateModelsOptions {
            auth_context: Some(ctx),
            ..Default::default()
        });
        models.set_provider(test_provider());
        models
    }

    #[test]
    fn models_get_available_filters_on_auth() {
        let models = test_models_with_env(None);
        let available = models.get_available(None);
        assert!(available.is_empty());
        let models = test_models_with_env(Some("env-key"));
        let available = models.get_available(None);
        assert_eq!(available.len(), 1);
    }

    #[test]
    fn models_check_auth_reflects_credentials() {
        let models = test_models_with_env(None);
        assert!(models.check_auth("test").is_none());
        assert!(models.check_auth("nope").is_none());
        let store = InMemoryCredentialStore::new();
        store.modify("test", &|_| Some(Credential::api_key("k")));
        let models = create_models(CreateModelsOptions {
            credentials: Some(Arc::new(store)),
            ..Default::default()
        });
        models.set_provider(test_provider());
        assert!(models.check_auth("test").is_some());
    }

    #[test]
    fn models_get_auth_resolves_env() {
        let models = test_models_with_env(Some("env-key"));
        let auth = models.get_auth("test", None).expect("resolves from env");
        assert_eq!(auth.auth.api_key.as_deref(), Some("env-key"));
        assert_eq!(auth.source.as_deref(), Some("TEST_API_KEY"));
    }

    #[test]
    fn models_get_auth_prefers_stored_credential() {
        let store = InMemoryCredentialStore::new();
        store.modify("test", &|_| Some(Credential::api_key("stored-key")));
        let models = create_models(CreateModelsOptions {
            credentials: Some(Arc::new(store)),
            ..Default::default()
        });
        models.set_provider(test_provider());
        let auth = models.get_auth("test", None).expect("resolves from stored");
        assert_eq!(auth.auth.api_key.as_deref(), Some("stored-key"));
        assert_eq!(auth.source.as_deref(), Some("stored"));
    }

    #[test]
    fn models_stream_applies_auth_and_dispatches() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let models = test_models_with_env(Some("env-key"));
            let model = models.get_model("test", "m1").expect("model");
            let ctx = Context::default();
            let options = StreamOptions::default();
            let msg = models
                .stream(&model, &ctx, Some(&options))
                .for_each(|_| {})
                .await;
            assert_eq!(msg.stop_reason(), Some(crate::types::StopReason::Stop));
        });
    }

    #[test]
    fn models_stream_error_when_provider_unconfigured() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let models = test_models_with_env(None);
            let model = models.get_model("test", "m1").expect("model");
            let ctx = Context::default();
            let options = StreamOptions::default();
            let msg = models
                .stream(&model, &ctx, Some(&options))
                .for_each(|_| {})
                .await;
            assert_eq!(msg.stop_reason(), Some(crate::types::StopReason::Error));
            assert!(msg.error_message().is_some());
        });
    }

    #[test]
    fn merge_headers_case_insensitive_override() {
        let base: ProviderHeaders = BTreeMap::from([
            ("x-test".to_string(), Some("1".to_string())),
            ("keep".to_string(), Some("y".to_string())),
        ]);
        let override_: ProviderHeaders =
            BTreeMap::from([("X-Test".to_string(), Some("2".to_string()))]);
        let merged = merge_headers(Some(&base), Some(&override_)).unwrap();
        assert_eq!(merged.get("X-Test"), Some(&Some("2".to_string())));
        assert_eq!(merged.get("keep"), Some(&Some("y".to_string())));
        assert!(!merged.contains_key("x-test"));
    }
}

#[cfg(test)]
mod oauth_auth_tests {
    use super::*;
    use crate::auth::{
        AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
    };

    struct TestOAuthAuth;
    #[async_trait::async_trait]
    impl OAuthAuth for TestOAuthAuth {
        fn name(&self) -> &str {
            "Test OAuth"
        }
        fn is_subscription(&self) -> bool {
            true
        }
        fn login_label(&self) -> Option<&str> {
            None
        }
        async fn login(
            &self,
            _interaction: &dyn AuthInteraction,
        ) -> Result<OAuthCredential, String> {
            Ok(OAuthCredential {
                refresh: "refresh-1".into(),
                access: "access-1".into(),
                expires: 1_800_000_000_000,
                extra: Default::default(),
            })
        }
        async fn refresh(
            &self,
            _credential: &OAuthCredential,
            _signal: &std::sync::atomic::AtomicBool,
        ) -> Result<OAuthCredential, String> {
            Err("not implemented".into())
        }
        fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
            Some(ModelAuth {
                api_key: Some(credential.access.clone()),
                base_url: Some("https://oauth-proxy.test".to_string()),
                headers: None,
            })
        }
    }

    fn oauth_provider() -> Provider {
        let stream = Arc::new(
            |_model: &Model, _ctx: &Context, _options: Option<&StreamOptions>| {
                crate::event_stream::create_error_stream("test", "test", "m1", "unused".to_string())
            },
        );
        let stream_simple = Arc::new(
            |_model: &Model, _ctx: &Context, _options: Option<&SimpleStreamOptions>| {
                crate::event_stream::create_error_stream("test", "test", "m1", "unused".to_string())
            },
        );
        create_provider(CreateProviderOptions {
            id: "oauth-test".to_string(),
            name: Some("OAuth Test".to_string()),
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: None,
                oauth: Some(Arc::new(TestOAuthAuth)),
            },
            models: vec![Model::new("m1", "M1", "test-api", "oauth-test")],
            api: ProviderApiSpec::Single(ProviderStreams {
                stream,
                stream_simple,
                fetch_deferred: None,
                cancel_deferred: None,
            }),
            filter_models: None,
        })
    }

    #[test]
    fn get_auth_derives_request_auth_from_oauth_credential() {
        let models = create_models(CreateModelsOptions::default());
        models.set_provider(oauth_provider());
        let cred = OAuthCredential {
            refresh: "refresh-1".into(),
            access: "access-1".into(),
            expires: 1_800_000_000_000,
            extra: Default::default(),
        };
        models
            .credentials
            .modify("oauth-test", &|_| Some(Credential::OAuth(cred.clone())));

        let auth = models
            .get_auth("oauth-test", None)
            .expect("oauth auth resolves");
        assert_eq!(auth.source.as_deref(), Some("OAuth"));
        assert_eq!(auth.auth.api_key.as_deref(), Some("access-1"));
        assert_eq!(
            auth.auth.base_url.as_deref(),
            Some("https://oauth-proxy.test")
        );
    }

    #[test]
    fn check_auth_reports_oauth_type() {
        let models = create_models(CreateModelsOptions::default());
        models.set_provider(oauth_provider());
        let cred = OAuthCredential {
            refresh: "r".into(),
            access: "a".into(),
            expires: 1_800_000_000_000,
            extra: Default::default(),
        };
        models
            .credentials
            .modify("oauth-test", &|_| Some(Credential::OAuth(cred.clone())));
        let check = models.check_auth("oauth-test").expect("auth check");
        assert_eq!(check.auth_type, "oauth");
        assert_eq!(check.source.as_deref(), Some("OAuth"));
    }

    #[test]
    fn get_available_includes_oauth_provider() {
        let models = create_models(CreateModelsOptions::default());
        models.set_provider(oauth_provider());
        let cred = OAuthCredential {
            refresh: "r".into(),
            access: "a".into(),
            expires: 1_800_000_000_000,
            extra: Default::default(),
        };
        models
            .credentials
            .modify("oauth-test", &|_| Some(Credential::OAuth(cred.clone())));
        let available = models.get_available(Some("oauth-test"));
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "m1");
    }

    #[test]
    fn auth_prompt_and_event_shapes() {
        let prompt = AuthPrompt::Text {
            message: "hi".into(),
            placeholder: None,
        };
        assert!(matches!(prompt, AuthPrompt::Text { .. }));
        let event = AuthEvent::DeviceCode {
            user_code: "ABCD".into(),
            verification_uri: "https://x".into(),
            interval_seconds: None,
            expires_in_seconds: Some(60),
        };
        assert!(matches!(event, AuthEvent::DeviceCode { .. }));
    }
}
