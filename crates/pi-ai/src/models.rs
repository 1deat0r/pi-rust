//! Models facade — port of `packages/ai/src/models.ts` + `models-store.ts`.
//!
//! `Provider` is the concrete runtime unit (id/name/base metadata, auth
//! methods, model listing, stream behavior). `Models` is the collection of
//! providers with auth application and stream convenience. `createModels`
//! builds a `Models`; `createProvider` builds a `Provider` from parts.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::join_all;

use crate::auth::{
    AuthCheck, AuthContext, AuthInteraction, AuthResult, Credential, CredentialStore,
    InMemoryCredentialStore, OAuthCredential, ProviderAuth, RuntimeCredentials,
};
use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::Model;
use crate::types::{
    AssistantMessage, Context, DeferredHandle, ProviderHeaders, ProviderRequestOptions,
    SimpleStreamOptions, StreamOptions,
};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn redact_oauth_error(error: &str, credential: &OAuthCredential) -> String {
    let mut redacted = error.to_string();
    for secret in [&credential.access, &credential.refresh] {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "<redacted>");
        }
    }
    redacted
}

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

/// Authentication method accepted by [`Models::login`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    ApiKey,
    OAuth,
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

/// A persistence mutation selected by a provider refresh. Keeping the
/// mutation separate from the in-memory update lets the Models facade commit
/// the file/store change before publishing a new synchronous model list.
#[derive(Debug, Clone)]
pub enum ModelsPersistence {
    Write(ModelsStoreEntry),
    Delete,
}

pub type ModelsUpdateFn = Arc<dyn Fn() + Send + Sync>;
#[derive(Default)]
pub struct ModelsPublication {
    pub persist: Option<ModelsPersistence>,
    pub update: Option<ModelsUpdateFn>,
}

type ModelsPublishFuture = Pin<Box<dyn Future<Output = Result<bool, String>> + Send>>;
type ModelsPublishFn =
    Arc<dyn Fn(ModelsPublication) -> ModelsPublishFuture + Send + Sync + 'static>;

/// Context passed to a dynamic provider's model refresh callback. The atomic
/// flag is the Rust equivalent of the upstream `AbortSignal` and is shared by
/// the HTTP provider and the publication gate.
#[derive(Clone)]
pub struct RefreshModelsContext {
    pub credential: Option<Credential>,
    pub stored: Option<ModelsStoreEntry>,
    publish_fn: ModelsPublishFn,
    pub allow_network: bool,
    pub force: bool,
    pub signal: Arc<AtomicBool>,
}

impl RefreshModelsContext {
    pub async fn publish(&self, publication: ModelsPublication) -> Result<bool, String> {
        (self.publish_fn)(publication).await
    }

    pub fn aborted(&self) -> bool {
        self.signal.load(Ordering::SeqCst)
    }
}

pub type RefreshModelsFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub type RefreshModelsFn =
    Arc<dyn Fn(RefreshModelsContext) -> RefreshModelsFuture + Send + Sync + 'static>;

pub type FetchModelsFuture = Pin<Box<dyn Future<Output = Result<Vec<Model>, String>> + Send>>;
pub type FetchModelsFn = Arc<dyn Fn(RefreshModelsContext) -> FetchModelsFuture + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ModelsRefreshOptions {
    pub allow_network: bool,
    pub providers: Option<Vec<String>>,
    pub force: bool,
    pub signal: Option<Arc<AtomicBool>>,
}

impl Default for ModelsRefreshOptions {
    fn default() -> Self {
        Self {
            allow_network: true,
            providers: None,
            force: false,
            signal: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: BTreeMap<String, ModelsError>,
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
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(provider_id)
            .cloned()
    }
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider_id.to_string(), entry.clone());
    }
    fn delete(&self, provider_id: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(provider_id);
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
pub type DeferredCancelFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub type DeferredCancelFn = Arc<
    dyn Fn(&Model, &DeferredHandle, &DeferredCancelOptions) -> DeferredCancelFuture + Send + Sync,
>;

/// Options for deferred fetch (upstream `DeferredFetchOptions`).
#[derive(Clone, Default)]
pub struct DeferredFetchOptions {
    pub base: crate::types::ProviderRequestOptions,
    /// Maximum provider long-poll duration. `None` performs one status check.
    pub wait: Option<u64>,
}

/// Request options for best-effort deferred cancellation.
pub type DeferredCancelOptions = crate::types::ProviderRequestOptions;

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
    /// Optional provider-owned dynamic catalog refresh. This field is kept on
    /// the provider so custom providers and built-ins share the same runtime
    /// merge semantics without a second registry.
    pub refresh_models: Option<RefreshModelsFn>,
    dynamic_models: Arc<RwLock<Vec<Model>>>,
}

impl Provider {
    /// Current known models, sync (mirrors upstream `currentModels`).
    pub fn get_models(&self) -> Vec<Model> {
        let dynamic = self
            .dynamic_models
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        crate::model_catalog::merge_model_lists(&self.models, &dynamic)
    }

    /// Replace the provider-owned dynamic overlay after a successful refresh.
    pub fn set_dynamic_models(&self, models: Vec<Model>) {
        *self
            .dynamic_models
            .write()
            .unwrap_or_else(|error| error.into_inner()) = models;
    }

    /// Return only the provider-owned dynamic overlay. This is useful to
    /// callers that need to distinguish a bundled model from a refreshed one.
    pub fn get_dynamic_models(&self) -> Vec<Model> {
        self.dynamic_models
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Attach an upstream-shaped refresh callback to a provider constructed by
    /// the existing `CreateProviderOptions` API.
    pub fn with_refresh_models(mut self, refresh_models: RefreshModelsFn) -> Self {
        self.refresh_models = Some(refresh_models);
        self
    }

    /// Attach a provider-owned dynamic catalog state and refresh callback.
    ///
    /// Most dynamic providers can use [`Provider::with_fetch_models`]. A
    /// provider such as Radius also needs to restore a legacy credential
    /// catalog during the cache-only phase, so its callback owns the exact
    /// publication behavior and shares the same state used by `get_models`.
    pub fn with_refresh_models_state(
        mut self,
        refresh_models: RefreshModelsFn,
        dynamic_models: Arc<RwLock<Vec<Model>>>,
    ) -> Self {
        self.dynamic_models = dynamic_models;
        self.refresh_models = Some(refresh_models);
        self
    }

    /// Attach a fetch callback. Cached entries are restored before the fetch;
    /// successful results are persisted and then published atomically.
    pub fn with_fetch_models(mut self, fetch_models: FetchModelsFn) -> Self {
        let dynamic_models = self.dynamic_models.clone();
        let provider_id = self.id.clone();
        self.refresh_models = Some(Arc::new(move |context| {
            let dynamic_models = dynamic_models.clone();
            let fetch_models = fetch_models.clone();
            let provider_id = provider_id.clone();
            Box::pin(async move {
                let restored = context
                    .stored
                    .as_ref()
                    .map(|stored| {
                        stored
                            .models
                            .iter()
                            .filter(|model| model.provider == provider_id)
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let restored_for_update = restored.clone();
                let restored_dynamic_models = dynamic_models.clone();
                if !context
                    .publish(ModelsPublication {
                        persist: None,
                        update: Some(Arc::new(move || {
                            *restored_dynamic_models
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) =
                                restored_for_update.clone();
                        })),
                    })
                    .await?
                {
                    return Ok(());
                }
                if !context.allow_network || context.aborted() {
                    return Ok(());
                }
                let refreshed = fetch_models(context.clone()).await?;
                if context.aborted() {
                    return Ok(());
                }
                let refreshed_for_update = refreshed.clone();
                let refreshed_dynamic_models = dynamic_models.clone();
                context
                    .publish(ModelsPublication {
                        persist: Some(ModelsPersistence::Write(ModelsStoreEntry {
                            models: refreshed.clone(),
                            last_modified: None,
                            checked_at: Some(now_ms()),
                            etag: None,
                        })),
                        update: Some(Arc::new(move || {
                            *refreshed_dynamic_models
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) =
                                refreshed_for_update.clone();
                        })),
                    })
                    .await?;
                Ok(())
            })
        }));
        self
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

fn is_expired_oauth_error(message: &AssistantMessage) -> bool {
    if message.stop_reason() != Some(crate::types::StopReason::Error) {
        return false;
    }
    let Some(error) = message.error_message() else {
        return false;
    };
    let lower = error.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("token expired")
        || lower.contains("expired token")
        || lower.contains("credential expired")
        || lower.contains("expired credential")
        || lower.contains("unauthorized")
}

async fn forward_stream_attempt(
    stream: AssistantMessageEventStream,
    pusher: &mut crate::event_stream::StreamSinkAdapter,
) -> (
    AssistantMessage,
    Option<crate::types::AssistantMessageEvent>,
) {
    let mut terminal = None;
    let final_message = stream
        .for_each(|event| {
            if matches!(
                &event,
                crate::types::AssistantMessageEvent::Done { .. }
                    | crate::types::AssistantMessageEvent::Error { .. }
            ) {
                terminal = Some(event);
            } else {
                pusher.push(event);
            }
        })
        .await;
    (final_message, terminal)
}

fn finish_stream_attempt(
    pusher: &mut crate::event_stream::StreamSinkAdapter,
    final_message: AssistantMessage,
    terminal: Option<crate::types::AssistantMessageEvent>,
) {
    if let Some(event) = terminal {
        pusher.push(event);
    } else if final_message.stop_reason().is_some() {
        pusher.end(Some(final_message));
    } else {
        pusher.end(None);
    }
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
        refresh_models: None,
        dynamic_models: Arc::new(RwLock::new(Vec::new())),
    }
}

/// Build a provider with an upstream-shaped dynamic catalog fetch callback.
/// This additive constructor keeps existing provider registrations source
/// compatible while giving custom providers the same refresh/runtime merge
/// behavior as built-in dynamic providers.
pub fn create_provider_with_fetch_models<F, Fut>(
    input: CreateProviderOptions,
    fetch_models: F,
) -> Provider
where
    F: Fn(RefreshModelsContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Model>, String>> + Send + 'static,
{
    let fetch_models: FetchModelsFn = Arc::new(move |context| Box::pin(fetch_models(context)));
    create_provider(input).with_fetch_models(fetch_models)
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
    providers: Arc<RwLock<BTreeMap<String, Provider>>>,
    /// Preserve the insertion order exposed by upstream's provider array.
    /// The map remains the authoritative lookup/index, while this sidecar
    /// keeps catalog and availability results stable for selectors and callers.
    provider_order: Arc<RwLock<Vec<String>>>,
    credentials: Arc<dyn CredentialStore>,
    runtime_credentials: Arc<RuntimeCredentials>,
    models_store: Arc<dyn ModelsStore>,
    auth_context: AuthContext,
    refresh_generations: Arc<Mutex<BTreeMap<String, u64>>>,
    refresh_signals: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
    publication_locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    oauth_refresh_locks: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

/// Build a Models collection (upstream `createModels`).
pub fn create_models(options: CreateModelsOptions) -> Models {
    let persistent_credentials = options
        .credentials
        .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new()));
    let runtime_credentials = Arc::new(RuntimeCredentials::new(persistent_credentials));
    Models {
        providers: Arc::new(RwLock::new(BTreeMap::new())),
        provider_order: Arc::new(RwLock::new(Vec::new())),
        credentials: runtime_credentials.clone(),
        runtime_credentials,
        models_store: options
            .models_store
            .unwrap_or_else(|| Arc::new(InMemoryModelsStore::new())),
        auth_context: options.auth_context.unwrap_or_default(),
        refresh_generations: Arc::new(Mutex::new(BTreeMap::new())),
        refresh_signals: Arc::new(Mutex::new(BTreeMap::new())),
        publication_locks: Arc::new(Mutex::new(BTreeMap::new())),
        oauth_refresh_locks: Arc::new(Mutex::new(BTreeMap::new())),
    }
}

impl Models {
    /// Set a non-persistent API key for this models runtime.  The key is used
    /// by auth checks and requests immediately, but is never written to the
    /// configured credential store.
    pub fn set_runtime_api_key(&self, provider_id: impl Into<String>, api_key: impl Into<String>) {
        self.runtime_credentials
            .set_runtime_api_key(provider_id, api_key);
    }

    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        self.runtime_credentials.remove_runtime_api_key(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.runtime_credentials.has_runtime_api_key(provider_id)
    }

    pub fn clear_runtime_api_keys(&self) {
        self.runtime_credentials.clear_runtime_api_keys();
    }

    pub fn set_provider(&self, provider: Provider) {
        self.supersede_provider_refresh(&provider.id);
        let provider_id = provider.id.clone();
        self.providers
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider_id.clone(), provider);
        let mut order = self
            .provider_order
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if !order.iter().any(|id| id == &provider_id) {
            order.push(provider_id);
        }
    }

    pub fn delete_provider(&self, id: &str) {
        self.supersede_provider_refresh(id);
        self.providers
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id);
        self.provider_order
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|entry| entry != id);
    }

    pub fn clear_providers(&self) {
        let ids = self
            .providers
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .chain(
                self.refresh_signals
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .keys()
                    .cloned(),
            )
            .collect::<Vec<_>>();
        for id in ids {
            self.supersede_provider_refresh(&id);
        }
        self.providers
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.provider_order
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub fn get_providers(&self) -> Vec<Provider> {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let order = self
            .provider_order
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut result = order
            .iter()
            .filter_map(|id| providers.get(id).cloned())
            .collect::<Vec<_>>();
        // Be defensive if provider state was populated without the sidecar.
        result.extend(
            providers
                .iter()
                .filter(|(id, _)| !order.iter().any(|ordered| ordered == *id))
                .map(|(_, provider)| provider.clone()),
        );
        result
    }

    pub fn get_provider(&self, id: &str) -> Option<Provider> {
        self.providers
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
    }

    /// Return the shared model-catalog store used by this facade. Coding-agent
    /// composition keeps this handle when it overlays models.json so refresh
    /// and runtime model overrides do not silently fall back to a new store.
    pub fn models_store(&self) -> Arc<dyn ModelsStore> {
        self.models_store.clone()
    }

    fn supersede_provider_refresh(&self, provider_id: &str) -> u64 {
        let mut generations = self
            .refresh_generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = generations.get(provider_id).copied().unwrap_or(0) + 1;
        generations.insert(provider_id.to_string(), generation);
        if let Some(signal) = self
            .refresh_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(provider_id)
        {
            signal.store(true, Ordering::SeqCst);
        }
        generation
    }

    fn begin_provider_refresh(&self, provider_id: &str) -> (u64, Arc<AtomicBool>) {
        let generation = self.supersede_provider_refresh(provider_id);
        let signal = Arc::new(AtomicBool::new(false));
        self.refresh_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider_id.to_string(), signal.clone());
        (generation, signal)
    }

    fn refresh_is_current(&self, provider_id: &str, generation: u64, signal: &AtomicBool) -> bool {
        !signal.load(Ordering::SeqCst)
            && self
                .refresh_generations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(provider_id)
                .copied()
                == Some(generation)
    }

    fn publication_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.publication_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn publish_provider_models(
        &self,
        provider_id: &str,
        generation: u64,
        signal: Arc<AtomicBool>,
        publication: ModelsPublication,
    ) -> Result<bool, String> {
        let lock = self.publication_lock(provider_id);
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        if !self.refresh_is_current(provider_id, generation, &signal) {
            return Ok(false);
        }
        if let Some(persist) = publication.persist {
            match persist {
                ModelsPersistence::Write(entry) => self.models_store.write(provider_id, &entry),
                ModelsPersistence::Delete => self.models_store.delete(provider_id),
            }
        }
        if !self.refresh_is_current(provider_id, generation, &signal) {
            return Ok(false);
        }
        if let Some(update) = publication.update {
            update();
        }
        Ok(true)
    }

    async fn wait_for_abort(signal: Arc<AtomicBool>) {
        while !signal.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    async fn refresh_one_provider(
        &self,
        provider: Provider,
        options: ModelsRefreshOptions,
        caller_signal: Arc<AtomicBool>,
    ) -> Result<(), ModelsError> {
        let Some(refresh_models) = provider.refresh_models.clone() else {
            return Ok(());
        };
        let provider_id = provider.id.clone();
        let (generation, signal) = self.begin_provider_refresh(&provider_id);
        let models = self.clone();
        let operation_signal = signal.clone();
        let operation_caller_signal = caller_signal.clone();
        let operation = async move {
            let stored = models.models_store.read(&provider_id);
            let publish_models = models.clone();
            let publish_provider_id = provider_id.clone();
            let publish_signal = operation_signal.clone();
            let publish_fn: ModelsPublishFn = Arc::new(move |publication| {
                let models = publish_models.clone();
                let provider_id = publish_provider_id.clone();
                let signal = publish_signal.clone();
                Box::pin(async move {
                    models
                        .publish_provider_models(&provider_id, generation, signal, publication)
                        .await
                })
            });
            let cache_context = RefreshModelsContext {
                // Dynamic providers may need the stored credential even in
                // the cache-only phase (Radius imports its legacy embedded
                // catalog before network access is considered).
                credential: models.credentials.read(&provider_id),
                stored: stored.clone(),
                publish_fn: publish_fn.clone(),
                allow_network: false,
                force: false,
                signal: operation_signal.clone(),
            };
            refresh_models(cache_context)
                .await
                .map_err(|error| ModelsError::new(ModelsErrorCode::ModelSource, error))?;
            if !options.allow_network
                || operation_caller_signal.load(Ordering::SeqCst)
                || !models.refresh_is_current(&provider_id, generation, &operation_signal)
            {
                return Ok(());
            }
            let credential = match models.credentials.read(&provider_id) {
                Some(Credential::OAuth(oauth)) => models
                    .resolve_oauth_credential(
                        &provider,
                        oauth,
                        operation_signal.clone(),
                        false,
                        None,
                    )
                    .await?
                    .map(Credential::OAuth),
                other => other,
            };
            if operation_caller_signal.load(Ordering::SeqCst)
                || !models.refresh_is_current(&provider_id, generation, &operation_signal)
            {
                return Ok(());
            }
            let network_context = RefreshModelsContext {
                credential,
                stored,
                publish_fn,
                allow_network: true,
                force: options.force,
                signal: operation_signal.clone(),
            };
            refresh_models(network_context)
                .await
                .map_err(|error| ModelsError::new(ModelsErrorCode::ModelSource, error))
        };
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => result,
            _ = Self::wait_for_abort(caller_signal.clone()) => {
                signal.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    /// Refresh all selected dynamic providers concurrently. Cache restoration
    /// always runs first; network refresh is optional and provider failures are
    /// returned per provider without discarding already-published models.
    pub async fn refresh(&self, options: ModelsRefreshOptions) -> ModelsRefreshResult {
        let caller_signal = options
            .signal
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let selected = options.providers.as_ref().map(|ids| {
            ids.iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        });
        let providers = self
            .get_providers()
            .into_iter()
            .filter(|provider| provider.refresh_models.is_some())
            .filter(|provider| {
                selected
                    .as_ref()
                    .map(|ids| ids.contains(&provider.id))
                    .unwrap_or(true)
            })
            .collect::<Vec<_>>();
        let results = join_all(providers.into_iter().map(|provider| {
            let provider_id = provider.id.clone();
            let provider_options = options.clone();
            let provider_signal = caller_signal.clone();
            async move {
                (
                    provider_id,
                    self.refresh_one_provider(provider, provider_options, provider_signal)
                        .await,
                )
            }
        }))
        .await;
        let mut errors = BTreeMap::new();
        for (provider_id, result) in results {
            if let Err(message) = result {
                errors.insert(provider_id, message);
            }
        }
        ModelsRefreshResult {
            aborted: caller_signal.load(Ordering::SeqCst),
            errors,
        }
    }

    /// Explicitly named alias for callers that prefer the operation name over
    /// the upstream `Models.refresh` spelling.
    pub async fn refresh_models(&self, options: ModelsRefreshOptions) -> ModelsRefreshResult {
        self.refresh(options).await
    }

    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        if let Some(provider_id) = provider {
            match self
                .providers
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(provider_id)
            {
                Some(p) => p.get_models(),
                None => Vec::new(),
            }
        } else {
            self.get_providers()
                .into_iter()
                .flat_map(|provider| provider.get_models())
                .collect()
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
        // A stored credential owns the provider. This prevents an invalid or
        // unsupported stored credential from silently falling back to an
        // unrelated ambient credential, matching upstream resolve semantics.
        match credential {
            Some(Credential::OAuth(_)) => {
                return provider.auth.oauth.as_ref().map(|_| AuthCheck {
                    source: Some("OAuth".to_string()),
                    auth_type: "oauth",
                });
            }
            Some(Credential::ApiKey(api_key_cred)) => {
                let api_key = provider.auth.api_key.as_ref()?;
                if let Some(check) = api_key.check(&self.auth_context, Some(api_key_cred)) {
                    return Some(check);
                }
                return self
                    .resolve_provider_auth(provider, Some(api_key_cred))
                    .map(|r| AuthCheck {
                        source: r.source,
                        auth_type: "api_key",
                    });
            }
            None => {}
        }
        let api_key = provider.auth.api_key.as_ref()?;
        if let Some(check) = api_key.check(&self.auth_context, None) {
            return Some(check);
        }
        self.resolve_provider_auth(provider, None)
            .map(|r| AuthCheck {
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
        self.get_auth_with_context(provider_id, model, &self.auth_context)
    }

    fn get_auth_with_context(
        &self,
        provider_id: &str,
        model: Option<&Model>,
        auth_context: &AuthContext,
    ) -> Option<AuthResult> {
        let provider = self.get_provider(provider_id)?;
        let credential = self.credentials.read(provider_id);
        // OAuth credentials derive request auth through the provider's OAuth
        // flow (upstream `getAuth` OAuth branch).
        let mut result = match credential.as_ref() {
            Some(Credential::OAuth(oauth_cred)) => {
                let oauth = provider.auth.oauth.as_ref()?;
                Some(AuthResult {
                    auth: oauth.to_auth(oauth_cred)?,
                    env: None,
                    source: Some("OAuth".to_string()),
                })
            }
            Some(Credential::ApiKey(api_key_cred)) => {
                self.resolve_provider_auth_with_context(&provider, Some(api_key_cred), auth_context)
            }
            None => self.resolve_provider_auth_with_context(&provider, None, auth_context),
        }?;
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
        self.resolve_provider_auth_with_context(provider, credential, &self.auth_context)
    }

    fn resolve_provider_auth_with_context(
        &self,
        provider: &Provider,
        credential: Option<&crate::auth::ApiKeyCredential>,
        auth_context: &AuthContext,
    ) -> Option<AuthResult> {
        let api_key = provider.auth.api_key.as_ref()?;
        api_key.resolve(auth_context, credential)
    }

    fn auth_context_with_env(&self, env: Option<&crate::types::ProviderEnv>) -> AuthContext {
        let Some(env) = env else {
            return self.auth_context.clone();
        };
        let scoped_env = env.clone();
        let ambient_env = self.auth_context.env.clone();
        let file_exists = self.auth_context.file_exists.clone();
        AuthContext {
            env: Arc::new(move |name| {
                scoped_env
                    .get(name)
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .or_else(|| (ambient_env)(name))
            }),
            file_exists,
        }
    }

    fn oauth_refresh_lock(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.oauth_refresh_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn resolve_oauth_credential(
        &self,
        provider: &Provider,
        stored: OAuthCredential,
        signal: Arc<AtomicBool>,
        force_refresh: bool,
        stale_credential: Option<&OAuthCredential>,
    ) -> Result<Option<OAuthCredential>, ModelsError> {
        const MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;
        let is_fresh = |credential: &OAuthCredential| {
            now_ms().saturating_add(MINIMUM_VALIDITY_MS) < credential.expires
        };
        if !force_refresh && is_fresh(&stored) {
            return Ok(Some(stored));
        }
        let Some(oauth) = provider.auth.oauth.clone() else {
            return Ok(None);
        };
        if signal.load(Ordering::SeqCst) {
            return Err(ModelsError::new(
                ModelsErrorCode::Oauth,
                format!("OAuth refresh cancelled for {}", provider.id),
            ));
        }

        // CredentialStore is synchronous in this Rust port, so the facade
        // supplies the same per-provider single-flight guarantee that the
        // upstream async store's modify transaction provides.
        let lock = self.oauth_refresh_lock(&provider.id);
        let _guard = tokio::select! {
            guard = lock.lock() => guard,
            _ = Self::wait_for_abort(signal.clone()) => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Oauth,
                    format!("OAuth refresh cancelled for {}", provider.id),
                ));
            }
        };
        let current = self.credentials.read(&provider.id);
        let Some(Credential::OAuth(current)) = current else {
            return Ok(None);
        };
        if !force_refresh && is_fresh(&current) {
            return Ok(Some(current));
        }
        // A second request may observe a token rotated by another stale
        // request while it was waiting for the per-provider lock. Reuse that
        // fresh token instead of forcing a second exchange.
        if force_refresh
            && stale_credential.is_some_and(|stale| stale != &current)
            && is_fresh(&current)
        {
            return Ok(Some(current));
        }
        let expected = current.clone();

        let refresh = oauth.refresh(&current, signal.as_ref());
        tokio::pin!(refresh);
        let refreshed = tokio::select! {
            result = &mut refresh => result,
            _ = Self::wait_for_abort(signal.clone()) => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Oauth,
                    format!("OAuth refresh cancelled for {}", provider.id),
                ));
            }
        };
        let refreshed = match refreshed {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let detail = redact_oauth_error(&error, &current);
                return Err(ModelsError::new(
                    ModelsErrorCode::Oauth,
                    format!("OAuth refresh failed for {}: {detail}", provider.id),
                ));
            }
        };

        // CredentialStore::modify is the atomic read-modify-write seam. The
        // expected-value guard prevents a logout/login or another writer from
        // being overwritten after the network exchange completes.
        let replacement = Credential::OAuth(refreshed.clone());
        let post = self.credentials.modify(&provider.id, &|current| {
            if matches!(current, Some(Credential::OAuth(current)) if current == &expected) {
                Some(replacement.clone())
            } else {
                None
            }
        });
        if let Some(Credential::OAuth(post)) = post {
            if post == refreshed || post != expected {
                return Ok(Some(post));
            }
        }

        // Stores that return no post-value for a no-op may still have been
        // refreshed by a concurrent writer. Read that result without ever
        // publishing our stale network response over it.
        match self.credentials.read(&provider.id) {
            Some(Credential::OAuth(current)) if current != expected && is_fresh(&current) => {
                Ok(Some(current))
            }
            _ => Ok(None),
        }
    }

    fn add_model_headers(result: &mut AuthResult, model: Option<&Model>) {
        if let Some(model) = model {
            if let Some(headers) = &model.headers {
                let headers_with_options: ProviderHeaders = headers
                    .iter()
                    .map(|(key, value)| (key.clone(), Some(value.clone())))
                    .collect();
                result.auth.headers =
                    merge_headers(result.auth.headers.as_ref(), Some(&headers_with_options));
            }
        }
    }

    /// Async auth resolution used by network-capable request paths. Unlike
    /// the legacy synchronous [`Models::get_auth`], this refreshes an OAuth
    /// credential inside the five-minute validity window and persists a
    /// rotated credential before returning request auth.
    pub async fn get_auth_async(
        &self,
        provider_id: &str,
        model: Option<&Model>,
        signal: Option<Arc<AtomicBool>>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        self.get_auth_async_with_refresh(provider_id, model, signal, false, None, None)
            .await
    }

    async fn get_auth_async_with_refresh(
        &self,
        provider_id: &str,
        model: Option<&Model>,
        signal: Option<Arc<AtomicBool>>,
        force_refresh: bool,
        stale_credential: Option<OAuthCredential>,
        env: Option<&crate::types::ProviderEnv>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        let signal = signal.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        if signal.load(Ordering::SeqCst) {
            return Err(ModelsError::new(
                ModelsErrorCode::Auth,
                "Authentication resolution cancelled",
            ));
        }
        let auth_context = self.auth_context_with_env(env);
        let credential = self.credentials.read(provider_id);
        let mut result = match credential {
            Some(Credential::OAuth(oauth)) => self
                .resolve_oauth_credential(
                    &provider,
                    oauth,
                    signal,
                    force_refresh,
                    stale_credential.as_ref(),
                )
                .await?
                .and_then(|oauth| {
                    provider
                        .auth
                        .oauth
                        .as_ref()?
                        .to_auth(&oauth)
                        .map(|auth| AuthResult {
                            auth,
                            env: None,
                            source: Some("OAuth".to_string()),
                        })
                }),
            Some(Credential::ApiKey(api_key)) => {
                self.resolve_provider_auth_with_context(&provider, Some(&api_key), &auth_context)
            }
            None => self.resolve_provider_auth_with_context(&provider, None, &auth_context),
        };
        if let Some(result) = result.as_mut() {
            Self::add_model_headers(result, model);
        }
        Ok(result)
    }

    /// Persist a provider credential after a real API-key or OAuth login.
    /// Provider failures are preserved verbatim; storage failures cannot be
    /// fabricated by the synchronous store abstraction and therefore never
    /// report success before the mutation returns.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, ModelsError> {
        let signal = interaction
            .signal()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        if signal.load(Ordering::SeqCst) {
            return Err(ModelsError::new(ModelsErrorCode::Auth, "Login cancelled"));
        }
        let provider = self.get_provider(provider_id).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {provider_id}"),
            )
        })?;
        let credential = match auth_type {
            AuthType::ApiKey => {
                let api_key = provider.auth.api_key.as_ref().ok_or_else(|| {
                    ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{} does not support api_key login", provider.name),
                    )
                })?;
                Credential::ApiKey(
                    api_key
                        .login(interaction)
                        .map_err(|error| ModelsError::new(ModelsErrorCode::Auth, error))?,
                )
            }
            AuthType::OAuth => {
                let oauth = provider.auth.oauth.as_ref().ok_or_else(|| {
                    ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{} does not support oauth login", provider.name),
                    )
                })?;
                let login = oauth.login(interaction);
                tokio::pin!(login);
                let oauth_credential = tokio::select! {
                    result = &mut login => result,
                    _ = Self::wait_for_abort(signal.clone()) => {
                        return Err(ModelsError::new(ModelsErrorCode::Auth, "Login cancelled"));
                    }
                }
                .map_err(|error| {
                    ModelsError::new(
                        ModelsErrorCode::Oauth,
                        format!("OAuth login failed for {provider_id}: {error}"),
                    )
                })?;
                Credential::OAuth(oauth_credential)
            }
        };
        if signal.load(Ordering::SeqCst) {
            return Err(ModelsError::new(ModelsErrorCode::Auth, "Login cancelled"));
        }
        self.credentials
            .modify(provider_id, &|_| Some(credential.clone()));
        Ok(credential)
    }

    /// Remove a persisted or runtime credential. Environment-based auth is
    /// intentionally unaffected, so a subsequent resolution may still use
    /// an ambient provider key after logout.
    pub async fn logout(&self, provider_id: &str) -> Result<(), ModelsError> {
        self.credentials.delete(provider_id);
        Ok(())
    }

    /// Async counterpart to [`Models::apply_auth`] used by all network
    /// request paths so an expiring OAuth credential is refreshed before the
    /// provider receives the request.
    pub async fn apply_auth_async(
        &self,
        model: &Model,
        options: &ProviderRequestOptions,
        signal: Option<Arc<AtomicBool>>,
    ) -> Result<(Model, ProviderRequestOptions), ModelsError> {
        self.apply_auth_async_with_refresh(model, options, signal, false, None)
            .await
    }

    async fn apply_auth_async_with_refresh(
        &self,
        model: &Model,
        options: &ProviderRequestOptions,
        signal: Option<Arc<AtomicBool>>,
        force_refresh: bool,
        stale_credential: Option<OAuthCredential>,
    ) -> Result<(Model, ProviderRequestOptions), ModelsError> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            ModelsError::new(
                ModelsErrorCode::UnknownProvider,
                format!("Unknown provider: {}", model.provider),
            )
        })?;
        let resolution = self
            .get_auth_async_with_refresh(
                &model.provider,
                Some(model),
                signal,
                force_refresh,
                stale_credential,
                options.env.as_ref(),
            )
            .await?;
        let resolution = match resolution {
            Some(resolution) => resolution,
            None if options
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
                || options.env.as_ref().is_some_and(|env| !env.is_empty()) =>
            {
                AuthResult {
                    auth: crate::auth::ModelAuth {
                        headers: model.headers.as_ref().map(|headers| {
                            headers
                                .iter()
                                .map(|(key, value)| (key.clone(), Some(value.clone())))
                                .collect()
                        }),
                        ..Default::default()
                    },
                    env: None,
                    source: Some("request override".to_string()),
                }
            }
            None => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Provider is not configured: {}", model.provider),
                ));
            }
        };
        let api_key = options
            .api_key
            .clone()
            .or_else(|| resolution.auth.api_key.clone());
        let headers = merge_headers(resolution.auth.headers.as_ref(), options.headers.as_ref());
        let env = match (&resolution.env, &options.env) {
            (None, None) => None,
            (Some(result), None) => Some(result.clone()),
            (None, Some(override_)) => Some(override_.clone()),
            (Some(result), Some(override_)) => {
                let mut merged = result.clone();
                merged.extend(override_.clone());
                Some(merged)
            }
        };
        let request_model = match &resolution.auth.base_url {
            Some(base_url) => {
                let mut request_model = model.clone();
                request_model.base_url = base_url.clone();
                request_model
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
        let _ = provider;
        Ok((request_model, request_options))
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
        let auth_context = self.auth_context_with_env(options.env.as_ref());
        let resolution = self.get_auth_with_context(&model.provider, Some(model), &auth_context);
        let resolution = match resolution {
            Some(resolution) => resolution,
            None if options
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
                || options.env.as_ref().is_some_and(|env| !env.is_empty()) =>
            {
                // Explicit request credentials are a valid auth override even
                // when the provider has no ambient environment/stored key.
                // Preserve static model headers so this path has the same
                // request shape as get_auth(model).
                let headers = model.headers.as_ref().map(|headers| {
                    headers
                        .iter()
                        .map(|(key, value)| (key.clone(), Some(value.clone())))
                        .collect()
                });
                AuthResult {
                    auth: crate::auth::ModelAuth {
                        headers,
                        ..Default::default()
                    },
                    env: None,
                    source: Some("request override".to_string()),
                }
            }
            None => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Provider is not configured: {}", model.provider),
                ));
            }
        };

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

    fn oauth_credential_for_retry(
        &self,
        provider_id: &str,
        options: Option<&StreamOptions>,
    ) -> Option<OAuthCredential> {
        // An explicit request key owns this request and must not trigger a
        // retry through an unrelated stored OAuth credential.
        if options.is_some_and(|options| options.base.api_key.is_some()) {
            return None;
        }
        let provider = self.get_provider(provider_id)?;
        provider.auth.oauth.as_ref()?;
        match self.credentials.read(provider_id) {
            Some(Credential::OAuth(credential)) => Some(credential),
            _ => None,
        }
    }

    async fn stream_with_auth_retry(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
        pusher: &mut crate::event_stream::StreamSinkAdapter,
    ) {
        let base_options = options
            .as_ref()
            .map(|options| options.base.clone())
            .unwrap_or_default();
        let stale_credential = self.oauth_credential_for_retry(&model.provider, options.as_ref());
        let result = self.apply_auth_async(model, &base_options, None).await;
        let (request_model, request_options) = match result {
            Ok(result) => result,
            Err(error) => {
                let message = error_message_for(model, &error.message);
                pusher.push(crate::types::AssistantMessageEvent::Error {
                    reason: crate::types::ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let Some(provider) = self.get_provider(&model.provider) else {
            let error = ModelsError::new(
                ModelsErrorCode::UnknownProvider,
                format!("Unknown provider: {}", model.provider),
            );
            let message = error_message_for(model, &error.message);
            pusher.push(crate::types::AssistantMessageEvent::Error {
                reason: crate::types::ErrorReason::Error,
                error_message: message.clone(),
            });
            pusher.end(Some(message));
            return;
        };
        let stream_options = options.clone().map(|mut options| {
            options.base = request_options.clone();
            options
        });
        let (first_message, first_terminal) = forward_stream_attempt(
            provider.stream(&request_model, context, stream_options.as_ref()),
            pusher,
        )
        .await;
        let retry = stale_credential.is_some()
            && first_terminal.as_ref().is_some_and(|event| {
                matches!(
                    event,
                    crate::types::AssistantMessageEvent::Error { error_message, .. }
                        if is_expired_oauth_error(error_message)
                )
            });
        if !retry {
            finish_stream_attempt(pusher, first_message, first_terminal);
            return;
        }

        let retry_result = self
            .apply_auth_async_with_refresh(model, &base_options, None, true, stale_credential)
            .await;
        let (retry_model, retry_options) = match retry_result {
            Ok(result) => result,
            Err(error) => {
                let message = error_message_for(model, &error.message);
                pusher.push(crate::types::AssistantMessageEvent::Error {
                    reason: crate::types::ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
                return;
            }
        };
        let retry_stream_options = options.map(|mut options| {
            options.base = retry_options;
            options
        });
        let (retry_message, retry_terminal) = forward_stream_attempt(
            provider.stream(&retry_model, context, retry_stream_options.as_ref()),
            pusher,
        )
        .await;
        finish_stream_attempt(pusher, retry_message, retry_terminal);
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
        let end_handle = outer.end_handle();
        let error_model = model.clone();
        tokio::spawn(async move {
            let mut pusher =
                crate::event_stream::StreamSinkAdapter::new_with_end(tx.clone(), end_handle);
            models
                .stream_with_auth_retry(&error_model, &context, options, &mut pusher)
                .await;
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
        let end_handle = outer.end_handle();
        let error_model = model.clone();
        tokio::spawn(async move {
            let mut pusher =
                crate::event_stream::StreamSinkAdapter::new_with_end(tx.clone(), end_handle);
            let base_options = options
                .as_ref()
                .map(|o| o.base.base.clone())
                .unwrap_or_default();
            let result = models.apply_auth_async(&model, &base_options, None).await;
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
        let (request_model, request_options) =
            self.apply_auth_async(model, &base_options, None).await?;
        let fetch_options = DeferredFetchOptions {
            base: request_options,
            wait: options.and_then(|o| o.wait),
        };
        let stream = fetcher(&request_model, handle, &fetch_options);
        Ok(stream.for_each(|_| {}).await)
    }

    /// Cancel a deferred response (upstream `Models.cancelDeferred`).
    pub async fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: Option<&DeferredCancelOptions>,
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
        let base_options = options.cloned().unwrap_or_default();
        let (request_model, request_options) =
            self.apply_auth_async(model, &base_options, None).await?;
        canceller(&request_model, handle, &request_options)
            .await
            .map_err(|e| ModelsError::new(ModelsErrorCode::Provider, e))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn models_preserve_provider_registration_order() {
        let models = create_models(CreateModelsOptions::default());
        let mut later = test_provider();
        later.id = "zeta".to_string();
        for model in &mut later.models {
            model.provider = "zeta".to_string();
        }
        let mut earlier = test_provider();
        earlier.id = "alpha".to_string();
        for model in &mut earlier.models {
            model.provider = "alpha".to_string();
        }

        models.set_provider(later);
        models.set_provider(earlier);
        assert_eq!(
            models
                .get_providers()
                .into_iter()
                .map(|provider| provider.id)
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn models_flatten_in_provider_registration_order() {
        let models = create_models(CreateModelsOptions::default());
        let mut first = test_provider();
        first.id = "zeta".to_string();
        for model in &mut first.models {
            model.provider = "zeta".to_string();
        }
        let mut second = test_provider();
        second.id = "alpha".to_string();
        for model in &mut second.models {
            model.provider = "alpha".to_string();
        }

        models.set_provider(first);
        models.set_provider(second);
        assert_eq!(
            models
                .get_models(None)
                .into_iter()
                .map(|model| model.provider)
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn models_available_preserves_provider_registration_order() {
        let models = test_models_with_env(Some("env-key"));
        let mut later = test_provider();
        later.id = "zeta".to_string();
        for model in &mut later.models {
            model.provider = "zeta".to_string();
        }
        let mut earlier = test_provider();
        earlier.id = "alpha".to_string();
        for model in &mut earlier.models {
            model.provider = "alpha".to_string();
        }

        models.delete_provider("test");
        models.set_provider(later);
        models.set_provider(earlier);
        assert_eq!(
            models
                .get_available(None)
                .into_iter()
                .map(|model| model.provider)
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
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
    fn runtime_api_key_overrides_without_mutating_persistent_store() {
        let store = InMemoryCredentialStore::new();
        store.modify("test", &|_| Some(Credential::api_key("stored-key")));
        let persistent = Arc::new(store.clone());
        let models = create_models(CreateModelsOptions {
            credentials: Some(persistent),
            ..Default::default()
        });
        models.set_provider(test_provider());

        models.set_runtime_api_key("test", "runtime-key");
        assert!(models.has_runtime_api_key("test"));
        assert_eq!(
            models
                .get_auth("test", None)
                .expect("runtime key resolves")
                .auth
                .api_key
                .as_deref(),
            Some("runtime-key")
        );
        assert_eq!(
            store.read("test"),
            Some(Credential::api_key("stored-key")),
            "runtime credentials must never write through"
        );
        assert!(models
            .credentials
            .list()
            .iter()
            .any(|entry| entry.provider_id == "test" && entry.credential_type == "api_key"));

        models.remove_runtime_api_key("test");
        assert!(!models.has_runtime_api_key("test"));
        assert_eq!(
            models
                .get_auth("test", None)
                .expect("persistent key is restored")
                .auth
                .api_key
                .as_deref(),
            Some("stored-key")
        );
    }

    #[test]
    fn empty_runtime_key_falls_back_and_delete_clears_override() {
        let store = InMemoryCredentialStore::new();
        store.modify("test", &|_| Some(Credential::api_key("stored-key")));
        let models = create_models(CreateModelsOptions {
            credentials: Some(Arc::new(store)),
            ..Default::default()
        });
        models.set_provider(test_provider());

        models.set_runtime_api_key("test", "");
        assert!(models.has_runtime_api_key("test"));
        assert_eq!(
            models
                .get_auth("test", None)
                .expect("empty override falls back")
                .auth
                .api_key
                .as_deref(),
            Some("stored-key")
        );
        models.set_runtime_api_key("test", "runtime-key");
        models.credentials.delete("test");
        assert!(!models.has_runtime_api_key("test"));
        assert!(models.get_auth("test", None).is_none());
    }

    #[test]
    fn env_api_key_auth_ignores_empty_values_and_reports_actual_source() {
        let auth = crate::auth::env_api_key_auth("Test", vec!["FIRST", "SECOND"]);
        let env = BTreeMap::from([
            ("FIRST".to_string(), "  ".to_string()),
            ("SECOND".to_string(), "second-key".to_string()),
        ]);
        let ctx = AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|_| false),
        };
        let check = auth.check(&ctx, None).expect("second env key is usable");
        assert_eq!(check.source.as_deref(), Some("SECOND"));
        let resolved = auth.resolve(&ctx, None).expect("second env key resolves");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("second-key"));

        let empty = BTreeMap::from([
            ("FIRST".to_string(), String::new()),
            ("SECOND".to_string(), "\t".to_string()),
        ]);
        let empty_ctx = AuthContext {
            env: Arc::new(move |name| empty.get(name).cloned()),
            file_exists: Arc::new(|_| false),
        };
        assert!(auth.check(&empty_ctx, None).is_none());
        assert!(auth.resolve(&empty_ctx, None).is_none());
    }

    #[test]
    fn apply_auth_accepts_explicit_request_credentials_without_ambient_auth() {
        let models = test_models_with_env(None);
        let model = models.get_model("test", "m1").expect("model");
        let options = ProviderRequestOptions {
            api_key: Some("explicit-key".to_string()),
            ..Default::default()
        };
        let (_, applied) = models
            .apply_auth(&model, &options)
            .expect("explicit key is a valid request override");
        assert_eq!(applied.api_key.as_deref(), Some("explicit-key"));

        let options = ProviderRequestOptions {
            env: Some(BTreeMap::from([(
                "TEST_API_KEY".to_string(),
                "scoped-key".to_string(),
            )])),
            ..Default::default()
        };
        let (_, applied) = models
            .apply_auth(&model, &options)
            .expect("scoped env is a valid request override");
        assert_eq!(applied.env, options.env);
    }

    #[test]
    fn apply_auth_scoped_env_overrides_ambient_and_explicit_key_wins() {
        let models = test_models_with_env(Some("ambient-key"));
        let model = models.get_model("test", "m1").expect("model");
        let scoped_env =
            BTreeMap::from([(String::from("TEST_API_KEY"), String::from("scoped-key"))]);
        let options = ProviderRequestOptions {
            env: Some(scoped_env.clone()),
            ..Default::default()
        };
        let (_, applied) = models
            .apply_auth(&model, &options)
            .expect("scoped env resolves provider auth");
        assert_eq!(applied.api_key.as_deref(), Some("scoped-key"));
        assert_eq!(applied.env, options.env);

        let options = ProviderRequestOptions {
            api_key: Some("explicit-key".to_string()),
            env: Some(scoped_env),
            ..Default::default()
        };
        let (_, applied) = models
            .apply_auth(&model, &options)
            .expect("explicit key overrides scoped auth");
        assert_eq!(applied.api_key.as_deref(), Some("explicit-key"));

        let debug = format!("{applied:?}");
        assert!(!debug.contains("explicit-key"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn apply_auth_async_resolves_request_scoped_env() {
        let models = test_models_with_env(Some("ambient-key"));
        let model = models.get_model("test", "m1").expect("model");
        let options = ProviderRequestOptions {
            env: Some(BTreeMap::from([(
                String::from("TEST_API_KEY"),
                String::from("scoped-key"),
            )])),
            ..Default::default()
        };

        let (_, applied) = models
            .apply_auth_async(&model, &options, None)
            .await
            .expect("scoped env resolves async provider auth");
        assert_eq!(applied.api_key.as_deref(), Some("scoped-key"));
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
