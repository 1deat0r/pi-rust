//! Auth types and helpers — port of `packages/ai/src/auth/types.ts` and
//! `auth/helpers.ts`.
//!
//! The Rust analog of the TS `Credential` union is the `Credential` enum;
//! `CredentialStore` is a trait so the app can back it with either an
//! in-memory map (tests) or the file-backed AuthStorage in pi-coding-agent.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::types::ProviderEnv;

/// Stored api-key credential. `env` holds provider-scoped environment/config
/// values (e.g. Cloudflare account/gateway ids).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// Stored canonical OAuth credential.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One type-tagged credential per provider — the shape of today's auth.json.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey(ApiKeyCredential),
    OAuth(OAuthCredential),
}

impl Credential {
    pub fn api_key(key: impl Into<String>) -> Self {
        Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
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
            env: Arc::new(|name| std::env::var(name).ok()),
            file_exists: Arc::new(|path| {
                let expanded = if let Some(rest) = path.strip_prefix("~/") {
                    std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or_else(|_| path.to_string())
                } else {
                    path.to_string()
                };
                std::path::Path::new(&expanded).exists()
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub base_url: Option<String>,
}

/// Result of resolving auth for a model.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub auth_type: &'static str, // "api_key" | "oauth"
}

/// Api-key auth: stored key/provider env plus ambient sources (env vars,
/// AWS profiles, ADC files).
pub trait ApiKeyAuth: Send + Sync {
    fn name(&self) -> &str;
    fn check(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthCheck>;
    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult>;
}

/// OAuth auth. Login/refresh flows run against the OAuth provider; the
/// `to_auth` step derives request auth from a stored credential.
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    fn is_subscription(&self) -> bool;
    fn login_label(&self) -> Option<&str>;
    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth>;
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
    fn modify(&self, provider_id: &str, f: &dyn Fn(Option<&Credential>) -> Option<Credential>) -> Option<Credential>;
    fn delete(&self, provider_id: &str);
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
        self.entries.lock().unwrap().get(provider_id).cloned()
    }

    fn list(&self) -> Vec<CredentialInfo> {
        self.entries
            .lock()
            .unwrap()
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

    fn modify(&self, provider_id: &str, f: &dyn Fn(Option<&Credential>) -> Option<Credential>) -> Option<Credential> {
        let mut entries = self.entries.lock().unwrap();
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
        self.entries.lock().unwrap().remove(provider_id);
    }
}

/// Standard api-key auth: a stored credential key wins, otherwise the first
/// set env var resolves (upstream `envApiKeyAuth`).
pub struct EnvApiKeyAuth {
    name: String,
    env_vars: Vec<String>,
}

pub fn env_api_key_auth(name: impl Into<String>, env_vars: Vec<impl Into<String>>) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars: env_vars.into_iter().map(|s| s.into()).collect(),
    })
}

impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, ctx: &AuthContext, _credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        if self.env_vars.iter().any(|v| ctx.env(v).is_some()) {
            Some(AuthCheck { source: Some(self.env_vars.first().cloned().unwrap_or_default()), auth_type: "api_key" })
        } else {
            None
        }
    }

    fn resolve(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        if let Some(cred) = credential {
            if cred.key.is_some() {
                return Some(AuthResult {
                    auth: ModelAuth { api_key: cred.key.clone(), headers: None, base_url: None },
                    env: cred.env.clone(),
                    source: Some("stored credential".to_string()),
                });
            }
        }
        for env_var in &self.env_vars {
            if let Some(value) = ctx.env(env_var) {
                return Some(AuthResult {
                    auth: ModelAuth { api_key: Some(value), headers: None, base_url: None },
                    env: None,
                    source: Some(env_var.clone()),
                });
            }
        }
        None
    }
}
