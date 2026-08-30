//! Auth types and helpers — port of `packages/ai/src/auth/types.ts` and
//! `auth/helpers.ts`.
//!
//! The Rust analog of the TS `Credential` union is the `Credential` enum;
//! `CredentialStore` is a trait so the app can back it with either an
//! in-memory map (tests) or the file-backed AuthStorage in pi-coding-agent.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{fmt, path::Path};

use crate::types::ProviderEnv;

/// Stored api-key credential. `env` holds provider-scoped environment/config
/// values (e.g. Cloudflare account/gateway ids).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

impl fmt::Debug for ApiKeyCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiKeyCredential")
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// Stored canonical OAuth credential.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("refresh", &"<redacted>")
            .field("access", &"<redacted>")
            .field("expires", &self.expires)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One type-tagged credential per provider — the shape of today's auth.json.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(credential) => formatter.debug_tuple("ApiKey").field(credential).finish(),
            Self::OAuth(credential) => formatter.debug_tuple("OAuth").field(credential).finish(),
        }
    }
}

impl Credential {
    pub fn api_key(key: impl Into<String>) -> Self {
        Credential::ApiKey(ApiKeyCredential {
            key: Some(key.into()),
            env: None,
        })
    }
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: &'static str,
}

/// Environment lookup closure for auth resolution.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// File-existence closure for auth resolution.
pub type FileExistsFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Environment access for auth resolution. Injectable for tests and browsers.
#[derive(Clone)]
pub struct AuthContext {
    pub env: EnvFn,
    pub file_exists: FileExistsFn,
}

impl Default for AuthContext {
    fn default() -> Self {
        Self {
            env: Arc::new(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            }),
            file_exists: Arc::new(|path| {
                let expanded = if let Some(rest) = path.strip_prefix('~') {
                    std::env::var("HOME")
                        .map(|home| format!("{home}{rest}"))
                        .unwrap_or_else(|_| path.to_string())
                } else {
                    path.to_string()
                };
                Path::new(&expanded).exists()
            }),
        }
    }
}

impl AuthContext {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn env(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }
    pub fn file_exists(&self, path: &str) -> bool {
        (self.file_exists)(path)
    }
}

/// Request auth for a single model request.
#[derive(Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub base_url: Option<String>,
}

impl fmt::Debug for ModelAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "header_names",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Result of resolving auth for a model.
#[derive(Clone, PartialEq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

impl fmt::Debug for AuthResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthResult")
            .field("auth", &self.auth)
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub auth_type: &'static str, // "api_key" | "oauth"
}

/// Stable categories for the string-based OAuth error boundary.
///
/// `OAuthAuth` deliberately keeps its upstream-compatible `String` error
/// return type.  Providers that can classify a failure include the
/// corresponding `[category]` marker in that string, while this enum gives
/// callers and diagnostics a non-secret way to interpret it.  The category
/// is never derived from or populated with credential material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFailureKind {
    Cancelled,
    InvalidGrant,
    Unauthorized,
    RateLimited,
    Server,
    Network,
    Timeout,
    MalformedResponse,
    AccountExtraction,
    Protocol,
    Unknown,
}

impl OAuthFailureKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidGrant => "invalid_grant",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Server => "server",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::MalformedResponse => "malformed_response",
            Self::AccountExtraction => "account_extraction",
            Self::Protocol => "protocol",
            Self::Unknown => "unknown",
        }
    }

    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Server | Self::Network | Self::Timeout
        )
    }

    pub const fn requires_relogin(self) -> bool {
        matches!(
            self,
            Self::InvalidGrant
                | Self::Unauthorized
                | Self::MalformedResponse
                | Self::AccountExtraction
        )
    }
}

/// Classify an OAuth error without attempting to inspect a credential.
/// Provider-specific errors should prefer the explicit `[category]` marker;
/// the text fallbacks keep cancellation and older provider messages useful to
/// callers during the transition to categorized diagnostics.
pub fn classify_oauth_failure(message: &str) -> OAuthFailureKind {
    let lower = message.to_ascii_lowercase();
    if lower == "login cancelled" || lower.contains("[cancelled]") {
        return OAuthFailureKind::Cancelled;
    }
    // Explicit provider markers are authoritative. Check them before the
    // human-readable fallbacks so a protocol message containing (for example)
    // the word "unauthorized" cannot change its stable category.
    if lower.contains("[invalid_grant]") {
        return OAuthFailureKind::InvalidGrant;
    }
    if lower.contains("[unauthorized]") {
        return OAuthFailureKind::Unauthorized;
    }
    if lower.contains("[rate_limited]") {
        return OAuthFailureKind::RateLimited;
    }
    if lower.contains("[server]") {
        return OAuthFailureKind::Server;
    }
    if lower.contains("[network]") {
        return OAuthFailureKind::Network;
    }
    if lower.contains("[timeout]") {
        return OAuthFailureKind::Timeout;
    }
    if lower.contains("[malformed_response]") {
        return OAuthFailureKind::MalformedResponse;
    }
    if lower.contains("[account_extraction]") {
        return OAuthFailureKind::AccountExtraction;
    }
    if lower.contains("[protocol]") {
        return OAuthFailureKind::Protocol;
    }

    if lower.contains("invalid_grant") {
        return OAuthFailureKind::InvalidGrant;
    }
    if lower.contains("unauthorized")
        || lower.contains("status 401")
        || lower.contains("(401)")
        || lower.contains("status 403")
        || lower.contains("(403)")
    {
        return OAuthFailureKind::Unauthorized;
    }
    if lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate_limited")
    {
        return OAuthFailureKind::RateLimited;
    }
    if lower.contains("service unavailable") || lower.contains("temporarily unavailable") {
        return OAuthFailureKind::Server;
    }
    if lower.contains("timed out") {
        return OAuthFailureKind::Timeout;
    }
    if lower.contains("invalid json") {
        return OAuthFailureKind::MalformedResponse;
    }
    if lower.contains("accountid") {
        return OAuthFailureKind::AccountExtraction;
    }
    if lower.contains("request failed") {
        return OAuthFailureKind::Network;
    }
    OAuthFailureKind::Unknown
}

/// Api-key auth: stored key/provider env plus ambient sources (env vars,
/// AWS profiles, ADC files).
pub trait ApiKeyAuth: Send + Sync {
    fn name(&self) -> &str;
    /// Standard interactive API-key setup. Ambient-only providers can leave
    /// this default in place by simply never calling it.
    fn login(&self, interaction: &dyn AuthInteraction) -> Result<ApiKeyCredential, String> {
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err("Login cancelled".to_string());
        }
        let key = interaction.prompt(&AuthPrompt::Secret {
            message: format!("Enter {}", self.name()),
            placeholder: None,
        })?;
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err("Login cancelled".to_string());
        }
        if key.trim().is_empty() {
            return Err("API key cannot be empty".to_string());
        }
        Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        })
    }
    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck>;
    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult>;
}

/// OAuth auth. Login/refresh flows run against the OAuth provider; the
/// `to_auth` step derives request auth from a stored credential.
#[async_trait::async_trait]
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    fn is_subscription(&self) -> bool;
    fn login_label(&self) -> Option<&str>;
    /// Run the interactive login flow (device code / callback server).
    /// Rejects on cancel/abort (upstream `login(interaction)`).
    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String>;
    /// Exchange the refresh token for a fresh credential. Network call;
    /// errors on failure (invalid_grant etc.). Runs under the store lock
    /// (upstream `refresh(credential, signal)`).
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &std::sync::atomic::AtomicBool,
    ) -> Result<OAuthCredential, String>;
    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth>;
}

// ---------------------------------------------------------------------------
// Login interaction (upstream `auth/types.ts` AuthPrompt/AuthEvent/
// AuthInteraction)
// ---------------------------------------------------------------------------

/// One prompt shown to the user during login.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthSelectOption>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthSelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// A link shown alongside an info/auth_url event.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

/// Out-of-band event notified to the UI during login.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<f64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// Login interaction callbacks serving both api-key and OAuth flows.
/// `prompt` returns the entered/selected string (`select` returns the option
/// id). Rejects on cancel/abort.
pub trait AuthInteraction: Send + Sync {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String>;
    fn notify(&self, event: &AuthEvent);

    /// Whole-login cancellation, analogous to upstream `AuthInteraction.signal`.
    /// Existing integrations may omit it; individual prompt/flow cancellation
    /// remains available through the methods below.
    fn signal(&self) -> Option<Arc<AtomicBool>> {
        None
    }

    /// Whether the interaction can keep an asynchronous terminal prompt live
    /// while an OAuth device or callback operation is pending.
    fn supports_async_prompt(&self) -> bool {
        false
    }

    /// Cancellable asynchronous prompt used by browser OAuth flows. The
    /// synchronous method remains the compatibility surface for providers
    /// whose prompts are ordinary terminal questions. Interactive UIs can
    /// override this method to keep a callback server and the prompt live at
    /// the same time.
    fn prompt_async_with_abort<'a>(
        &'a self,
        prompt: &'a AuthPrompt,
        abort: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if abort.load(Ordering::SeqCst) {
                return Err("Login cancelled".to_string());
            }
            self.prompt(prompt)
        })
    }
}

/// A no-op interaction for headless flows (tests, non-interactive login).
/// `prompt` always errors with `message`.
pub struct NoopAuthInteraction {
    pub error_message: String,
}

impl NoopAuthInteraction {
    pub fn new() -> Self {
        Self {
            error_message: "login requires an interactive prompt".to_string(),
        }
    }
}

impl Default for NoopAuthInteraction {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthInteraction for NoopAuthInteraction {
    fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, String> {
        Err(self.error_message.clone())
    }
    fn notify(&self, _event: &AuthEvent) {}
}

/// Provider auth. At least one of apiKey/oauth is present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

/// App-owned credential storage, keyed by Provider.id, one credential per
/// provider. `modify` is the only write path.
pub trait CredentialStore: Send + Sync {
    fn read(&self, provider_id: &str) -> Option<Credential>;
    fn list(&self) -> Vec<CredentialInfo>;
    fn modify(
        &self,
        provider_id: &str,
        f: &dyn Fn(Option<&Credential>) -> Option<Credential>,
    ) -> Option<Credential>;
    fn delete(&self, provider_id: &str);
}

/// A non-persistent API-key overlay for long-lived runtimes.
///
/// This is the Rust equivalent of coding-agent's `RuntimeCredentials`.  It
/// is deliberately layered over a normal [`CredentialStore`]: reads and
/// availability checks see an in-memory key immediately, while writes still
/// go to the underlying store.  The overlay is never serialized, so a
/// runtime credential cannot leak into `auth.json`, sessions, or exports.
#[derive(Clone)]
pub struct RuntimeCredentials {
    store: Arc<dyn CredentialStore>,
    overrides: Arc<Mutex<BTreeMap<String, String>>>,
}

impl RuntimeCredentials {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self {
            store,
            overrides: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Set or replace the key used for this runtime only.
    pub fn set_runtime_api_key(&self, provider_id: impl Into<String>, api_key: impl Into<String>) {
        self.overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider_id.into(), api_key.into());
    }

    /// Remove the runtime override and reveal the persistent/environment
    /// credential again.
    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        self.overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(provider_id)
    }

    /// Clear every runtime-only override.  Useful at session shutdown and for
    /// tests that verify no credential survives a runtime boundary.
    pub fn clear_runtime_api_keys(&self) {
        self.overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

impl CredentialStore for RuntimeCredentials {
    fn read(&self, provider_id: &str) -> Option<Credential> {
        let override_key = self
            .overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(provider_id)
            .cloned();
        // Match the upstream truthy override behavior: an empty runtime key
        // is not usable and falls through to the underlying store.
        if let Some(key) = override_key.as_deref() {
            if !key.is_empty() {
                return Some(Credential::api_key(key.to_string()));
            }
        }
        self.store.read(provider_id)
    }

    fn list(&self) -> Vec<CredentialInfo> {
        let mut entries = self
            .store
            .list()
            .into_iter()
            .map(|entry| (entry.provider_id.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        for provider_id in self
            .overrides
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
        {
            entries.insert(
                provider_id.clone(),
                CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: "api_key",
                },
            );
        }
        entries.into_values().collect()
    }

    fn modify(
        &self,
        provider_id: &str,
        f: &dyn Fn(Option<&Credential>) -> Option<Credential>,
    ) -> Option<Credential> {
        self.store.modify(provider_id, f)
    }

    fn delete(&self, provider_id: &str) {
        self.store.delete(provider_id);
        self.remove_runtime_api_key(provider_id);
    }
}

/// In-memory credential store (upstream `InMemoryCredentialStore`). Interior
/// mutability so the Models facade can hold it behind a shared reference.
#[derive(Default, Clone)]
pub struct InMemoryCredentialStore {
    entries: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Credential>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read(&self, provider_id: &str) -> Option<Credential> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(provider_id)
            .cloned()
    }

    fn list(&self) -> Vec<CredentialInfo> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(id, c)| CredentialInfo {
                provider_id: id.clone(),
                credential_type: match c {
                    Credential::ApiKey(_) => "api_key",
                    Credential::OAuth(_) => "oauth",
                },
            })
            .collect()
    }

    fn modify(
        &self,
        provider_id: &str,
        f: &dyn Fn(Option<&Credential>) -> Option<Credential>,
    ) -> Option<Credential> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = entries.get(provider_id);
        match f(current) {
            Some(new) => {
                entries.insert(provider_id.to_string(), new.clone());
                Some(new)
            }
            None => current.cloned(),
        }
    }

    fn delete(&self, provider_id: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(provider_id);
    }
}

/// Standard api-key auth: a stored credential key wins, otherwise the first
/// set env var resolves (upstream `envApiKeyAuth`).
pub struct EnvApiKeyAuth {
    name: String,
    env_vars: Vec<String>,
}

pub fn env_api_key_auth(
    name: impl Into<String>,
    env_vars: Vec<impl Into<String>>,
) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars: env_vars.into_iter().map(|s| s.into()).collect(),
    })
}

impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        if credential
            .and_then(|credential| credential.key.as_deref())
            .is_some_and(|key| !key.trim().is_empty())
        {
            return Some(AuthCheck {
                source: Some("stored credential".to_string()),
                auth_type: "api_key",
            });
        }
        self.env_vars.iter().find_map(|env_var| {
            let value = ctx.env(env_var)?;
            (!value.trim().is_empty()).then(|| AuthCheck {
                source: Some(env_var.clone()),
                auth_type: "api_key",
            })
        })
    }

    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult> {
        if let Some(cred) = credential {
            if cred
                .key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
            {
                return Some(AuthResult {
                    auth: ModelAuth {
                        api_key: cred.key.clone(),
                        headers: None,
                        base_url: None,
                    },
                    env: cred.env.clone(),
                    source: Some("stored credential".to_string()),
                });
            }
        }
        for env_var in &self.env_vars {
            if let Some(value) = ctx.env(env_var).filter(|value| !value.trim().is_empty()) {
                return Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some(env_var.clone()),
                });
            }
        }
        None
    }
}
