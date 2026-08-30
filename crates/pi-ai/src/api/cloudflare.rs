//! Cloudflare base URLs, auth, stream wrapping, and AI Gateway binding
//! transport — port of `packages/ai/src/api/cloudflare.ts`,
//! `cloudflare-auth.ts`, `cloudflare-stream.ts`, and
//! `cloudflare-gateway-binding.ts`.
//!
//! Cloudflare's two providers reuse the standard API adaptors (anthropic-
//! messages / openai-completions / openai-responses) against gateway /
//! Workers-AI endpoints whose URLs contain `{CLOUDFLARE_ACCOUNT_ID}` /
//! `{CLOUDFLARE_GATEWAY_ID}` placeholders; the placeholders materialize from
//! the resolved provider env before dispatch. Auth accepts a stored
//! credential or ambient `CLOUDFLARE_API_KEY` plus account (and gateway)
//! ids, per-field merged.
//!
//! The binding transport is runtime-neutral: `create_gateway_binding_request`
//! validates and translates an effective HTTPS request into the universal
//! `gateway(id).run(...)` payload, while `AiGatewayBinding` lets a
//! Cloudflare-Workers adapter execute that payload without coupling this crate
//! to Workers-only types. Requests outside the configured prefix or unsupported
//! by the universal endpoint reject instead of silently falling back to an
//! authenticated HTTPS request.

use std::collections::BTreeMap;
use std::sync::{atomic::AtomicBool, Arc};

use url::Url;

use crate::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthResult, ModelAuth, ProviderAuth,
};
use crate::error::PiAiError;
use crate::model::Model;
use crate::models::ProviderStreams;
use crate::types::{ProviderEnv, ProviderHeaders};

/// Structural request accepted by the Workers AI universal gateway binding.
#[derive(Debug, Clone, PartialEq)]
pub struct AiGatewayUniversalRequest {
    pub provider: String,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub query: serde_json::Value,
}

/// Effective HTTP request presented to the binding transport translator.
///
/// Callers that model the JavaScript `fetch(input, init)` contract should
/// resolve `init` first: an init method/header/body replaces the corresponding
/// `Request` field, while an omitted init field preserves it.
#[derive(Debug, Clone)]
pub struct GatewayBindingFetchRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a BTreeMap<String, String>,
    pub body: Option<&'a [u8]>,
    /// Runtime-neutral cancellation flag; `None` means no abort signal.
    pub signal: Option<Arc<AtomicBool>>,
}

/// Gateway HTTPS prefix and binding gateway name for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayBindingFetchOptions {
    pub base_url: String,
    pub gateway: String,
}

/// Runtime adapter for `env.AI.gateway(id).run(...)`.
///
/// The associated response is intentionally opaque so a Workers integration
/// can return its native streaming response untouched.
#[async_trait::async_trait]
pub trait AiGatewayBinding: Send + Sync {
    type Response: Send;

    async fn run(
        &self,
        gateway: &str,
        request: AiGatewayUniversalRequest,
        signal: Option<Arc<AtomicBool>>,
    ) -> Result<Self::Response, String>;
}

/// Translate an HTTPS gateway request into a universal binding request.
///
/// Only POST requests with a JSON body under the configured origin/path prefix
/// are expressible by the universal endpoint. Header names are lowercased and
/// `content-length`, `host`, and `cf-aig-authorization` are omitted before
/// forwarding.
pub fn create_gateway_binding_request(
    options: &GatewayBindingFetchOptions,
    request: &GatewayBindingFetchRequest<'_>,
) -> Result<AiGatewayUniversalRequest, String> {
    let method = request.method.to_ascii_uppercase();
    let base = Url::parse(&options.base_url).map_err(|error| {
        format!(
            "createGatewayBindingFetch: invalid configured gateway prefix {}: {error}",
            options.base_url
        )
    })?;
    let parsed = Url::parse(request.url)
        .map_err(|_| outside_gateway_prefix_error(&method, request.url, &options.base_url))?;
    let base_path = gateway_prefix_path(base.path());
    let request_path = normalize_url_path(parsed.path());
    if !same_origin(&parsed, &base) || !request_path.starts_with(&base_path) {
        return Err(outside_gateway_prefix_error(
            &method,
            request.url,
            &options.base_url,
        ));
    }
    if method != "POST" {
        return Err(cannot_express_gateway_request(
            &method,
            request.url,
            "only POST is supported",
        ));
    }

    let rest = &request_path[base_path.len()..];
    let Some(slash) = rest.find('/') else {
        return Err(cannot_express_gateway_request(
            &method,
            request.url,
            "missing provider/endpoint path",
        ));
    };
    if slash == 0 {
        return Err(cannot_express_gateway_request(
            &method,
            request.url,
            "missing provider/endpoint path",
        ));
    }
    let provider = &rest[..slash];
    let mut endpoint = rest[slash + 1..].to_string();
    if let Some(query) = parsed.query().filter(|query| !query.is_empty()) {
        endpoint.push('?');
        endpoint.push_str(query);
    }

    let Some(body) = request.body else {
        return Err(cannot_express_gateway_request(
            &method,
            request.url,
            "missing body",
        ));
    };
    let query = serde_json::from_slice(body)
        .map_err(|_| cannot_express_gateway_request(&method, request.url, "non-JSON body"))?;

    let mut headers = BTreeMap::new();
    for (name, value) in request.headers {
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "content-length" | "host" | "cf-aig-authorization"
        ) {
            continue;
        }
        headers.insert(name, value.clone());
    }

    Ok(AiGatewayUniversalRequest {
        provider: provider.to_string(),
        endpoint,
        headers,
        query,
    })
}

/// Translate and execute one request through a Workers AI gateway binding.
pub async fn run_gateway_binding_request<B: AiGatewayBinding>(
    binding: &B,
    options: &GatewayBindingFetchOptions,
    request: &GatewayBindingFetchRequest<'_>,
) -> Result<B::Response, String> {
    let universal = create_gateway_binding_request(options, request)?;
    binding
        .run(&options.gateway, universal, request.signal.clone())
        .await
}

fn outside_gateway_prefix_error(method: &str, url: &str, base_url: &str) -> String {
    format!(
        "createGatewayBindingFetch: {method} {url} is outside the configured gateway prefix \
         ({base_url}); this fetch only serves its gateway-bound client"
    )
}

fn cannot_express_gateway_request(method: &str, url: &str, reason: &str) -> String {
    format!(
        "createGatewayBindingFetch: cannot express {method} {url} as a universal gateway \
         request ({reason}); route it over HTTPS with gateway auth instead"
    )
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn gateway_prefix_path(path: &str) -> String {
    let mut normalized = normalize_url_path(path);
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

fn normalize_url_path(path: &str) -> String {
    let mut segments = Vec::new();
    let mut raw_segments = path.split('/').peekable();
    while let Some(segment) = raw_segments.next() {
        let is_last = raw_segments.peek().is_none();
        if is_single_dot_segment(segment) {
            if is_last {
                segments.push("");
            }
        } else if is_double_dot_segment(segment) {
            if segments.len() > 1 {
                segments.pop();
            }
            if is_last {
                segments.push("");
            }
        } else {
            segments.push(segment);
        }
    }
    let normalized = segments.join("/");
    if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized
    }
}

fn is_single_dot_segment(segment: &str) -> bool {
    segment == "." || segment.eq_ignore_ascii_case("%2e")
}

fn is_double_dot_segment(segment: &str) -> bool {
    segment == ".."
        || segment.eq_ignore_ascii_case(".%2e")
        || segment.eq_ignore_ascii_case("%2e.")
        || segment.eq_ignore_ascii_case("%2e%2e")
}

/// Workers AI direct endpoint.
pub const CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";

/// AI Gateway Unified API.
pub const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";

/// AI Gateway → OpenAI passthrough.
pub const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";

/// AI Gateway → Anthropic passthrough.
pub const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";

pub const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
pub const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
pub const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

/// Placeholder values for auth headers on binding-routed requests. Binding
/// calls are pre-authenticated; HTTPS callers use a real API key. Kept for
/// parity with upstream's sentinel constant.
pub const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL: &str = "cloudflare-gateway-binding";

/// Which Cloudflare endpoint family an auth resolves for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudflareAuthKind {
    WorkersAi,
    AiGateway,
}

fn get_env_value(name: &str, env: Option<&ProviderEnv>, ctx: &AuthContext) -> Option<String> {
    if let Some(env) = env {
        if let Some(v) = env.get(name) {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    ctx.env(name).filter(|v| !v.is_empty())
}

/// Per-field merge: prefer the credential value, falling back to ambient env
/// only when the credential does not define that field (upstream
/// `resolveValue`). An explicitly empty credential field still blocks ambient
/// fallback and is rejected by `resolved`.
fn resolve_value(
    name: &str,
    ctx: &AuthContext,
    credential: Option<&ApiKeyCredential>,
) -> Option<String> {
    let from_credential = credential.and_then(|c| {
        if name == CLOUDFLARE_API_KEY {
            c.key.clone()
        } else {
            c.env.as_ref().and_then(|e| e.get(name)).cloned()
        }
    });
    match from_credential {
        Some(v) => Some(v),
        None => get_env_value(name, None, ctx),
    }
}

/// Cloudflare api-key auth (upstream `cloudflareWorkersAIAuth` /
/// `cloudflareAIGatewayAuth`). Requires the API key + account id (and
/// gateway id for the AI Gateway kind) before resolving.
pub struct CloudflareAuth {
    kind: CloudflareAuthKind,
}

/// Build a `ProviderAuth` carrying Cloudflare api-key auth for the given
/// endpoint family.
pub fn cloudflare_auth(kind: CloudflareAuthKind) -> ProviderAuth {
    ProviderAuth {
        api_key: Some(Arc::new(CloudflareAuth { kind })),
        oauth: None,
    }
}

impl CloudflareAuth {
    fn resolved(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<(String, ProviderEnv, String)> {
        let api_key =
            resolve_value(CLOUDFLARE_API_KEY, ctx, credential).filter(|v| !v.is_empty())?;
        let account_id =
            resolve_value(CLOUDFLARE_ACCOUNT_ID, ctx, credential).filter(|v| !v.is_empty())?;
        let gateway_id = match self.kind {
            CloudflareAuthKind::WorkersAi => None,
            CloudflareAuthKind::AiGateway => Some(
                resolve_value(CLOUDFLARE_GATEWAY_ID, ctx, credential).filter(|v| !v.is_empty())?,
            ),
        };
        let mut env = ProviderEnv::new();
        env.insert(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id);
        if let Some(gateway_id) = gateway_id {
            env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
        }
        Some((
            api_key,
            env,
            if credential.is_some() {
                "stored credential".to_string()
            } else {
                CLOUDFLARE_API_KEY.to_string()
            },
        ))
    }
}

impl ApiKeyAuth for CloudflareAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
    }

    fn login(
        &self,
        interaction: &dyn crate::auth::AuthInteraction,
    ) -> Result<ApiKeyCredential, PiAiError> {
        let key = interaction
            .prompt(&crate::auth::AuthPrompt::Secret {
                message: "Enter Cloudflare API key".to_string(),
                placeholder: None,
            })
            .map_err(|e| e.to_string())?;
        let account_id = interaction
            .prompt(&crate::auth::AuthPrompt::Text {
                message: "Enter Cloudflare account ID".to_string(),
                placeholder: None,
            })
            .map_err(|e| e.to_string())?;
        let mut env = ProviderEnv::from([(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id)]);
        if self.kind == CloudflareAuthKind::AiGateway {
            let gateway_id = interaction
                .prompt(&crate::auth::AuthPrompt::Text {
                    message: "Enter Cloudflare AI Gateway ID".to_string(),
                    placeholder: None,
                })
                .map_err(|e| e.to_string())?;
            env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
        }
        Ok(ApiKeyCredential {
            key: Some(key),
            env: Some(env),
        })
    }

    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        self.resolved(ctx, credential)
            .map(|(_, _, source)| AuthCheck {
                source: Some(source),
                auth_type: "api_key",
            })
    }

    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult> {
        let (api_key, env, source) = self.resolved(ctx, credential)?;
        let auth = match self.kind {
            CloudflareAuthKind::WorkersAi => ModelAuth {
                api_key: Some(api_key),
                headers: None,
                base_url: None,
            },
            CloudflareAuthKind::AiGateway => {
                let mut headers = ProviderHeaders::new();
                headers.insert(
                    "cf-aig-authorization".to_string(),
                    Some(format!("Bearer {api_key}")),
                );
                headers.insert("Authorization".to_string(), None);
                headers.insert("x-api-key".to_string(), None);
                ModelAuth {
                    api_key: None,
                    headers: Some(headers),
                    base_url: None,
                }
            }
        };
        Some(AuthResult {
            auth,
            env: Some(env),
            source: Some(source),
        })
    }
}

/// Substitute `{CLOUDFLARE_ACCOUNT_ID}` / `{CLOUDFLARE_GATEWAY_ID}`
/// placeholders in `model.baseUrl` from the provider env (upstream
/// `resolveCloudflareModel`). Unresolved placeholders are kept verbatim.
pub fn resolve_cloudflare_model(model: &Model, env: Option<&ProviderEnv>) -> Model {
    let Some(env) = env else { return model.clone() };
    let account = env
        .get(CLOUDFLARE_ACCOUNT_ID)
        .cloned()
        .unwrap_or_else(|| format!("{{{CLOUDFLARE_ACCOUNT_ID}}}"));
    let gateway = env
        .get(CLOUDFLARE_GATEWAY_ID)
        .cloned()
        .unwrap_or_else(|| format!("{{{CLOUDFLARE_GATEWAY_ID}}}"));
    let base_url = model
        .base_url
        .replace(&format!("{{{CLOUDFLARE_ACCOUNT_ID}}}"), &account)
        .replace(&format!("{{{CLOUDFLARE_GATEWAY_ID}}}"), &gateway);
    if base_url == model.base_url {
        model.clone()
    } else {
        let mut resolved = model.clone();
        resolved.base_url = base_url;
        resolved
    }
}

/// Wrap an API implementation so Cloudflare account/gateway endpoint
/// placeholders materialize from the resolved provider env before dispatch
/// (upstream `cloudflareStreams`).
pub fn cloudflare_streams(inner: ProviderStreams) -> ProviderStreams {
    let stream: crate::models::StreamFn = {
        let inner = inner.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>|
                  -> crate::event_stream::AssistantMessageEventStream {
                let env = options.and_then(|o| o.base.env.as_ref());
                let resolved = resolve_cloudflare_model(model, env);
                let f = inner.stream.clone();
                f(&resolved, ctx, options)
            },
        )
    };
    let stream_simple: crate::models::SimpleStreamFn = {
        let inner = inner.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>|
                  -> crate::event_stream::AssistantMessageEventStream {
                let env = options.and_then(|o| o.base.base.env.as_ref());
                let resolved = resolve_cloudflare_model(model, env);
                let f = inner.stream_simple.clone();
                f(&resolved, ctx, options)
            },
        )
    };
    ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::event_stream::AssistantMessageEventStream;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn cloudflare_model() -> Model {
        let mut model = Model::new(
            "model",
            "model",
            "openai-completions",
            "cloudflare-ai-gateway",
        );
        model.base_url = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai".to_string();
        model
    }

    #[test]
    fn resolves_placeholders_from_env() {
        let model = cloudflare_model();
        let mut env = ProviderEnv::new();
        env.insert("CLOUDFLARE_ACCOUNT_ID".to_string(), "account".to_string());
        env.insert("CLOUDFLARE_GATEWAY_ID".to_string(), "gateway".to_string());
        let resolved = resolve_cloudflare_model(&model, Some(&env));
        assert_eq!(
            resolved.base_url,
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai"
        );
    }

    #[test]
    fn keeps_placeholders_when_env_absent() {
        let model = cloudflare_model();
        let resolved = resolve_cloudflare_model(&model, None);
        assert_eq!(resolved.base_url, model.base_url);
    }

    #[test]
    fn cloudflare_streams_materializes_endpoint_before_dispatch() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured_for_stream = captured.clone();
        let captured_for_simple = captured.clone();
        let stream_fn: crate::models::StreamFn = Arc::new(
            move |model: &Model,
                  _ctx: &crate::types::Context,
                  _o: Option<&crate::types::StreamOptions>|
                  -> AssistantMessageEventStream {
                captured_for_stream
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(model.base_url.clone());
                AssistantMessageEventStream::new()
            },
        );
        let simple_fn: crate::models::SimpleStreamFn = Arc::new(
            move |model: &Model,
                  _ctx: &crate::types::Context,
                  _o: Option<&crate::types::SimpleStreamOptions>|
                  -> AssistantMessageEventStream {
                captured_for_simple
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(model.base_url.clone());
                AssistantMessageEventStream::new()
            },
        );
        let inner = ProviderStreams {
            stream: stream_fn,
            stream_simple: simple_fn,
            fetch_deferred: None,
            cancel_deferred: None,
        };
        let wrapped = cloudflare_streams(inner);
        let model = cloudflare_model();
        let ctx = crate::types::Context::default();
        let mut env = ProviderEnv::new();
        env.insert("CLOUDFLARE_ACCOUNT_ID".to_string(), "account".to_string());
        env.insert("CLOUDFLARE_GATEWAY_ID".to_string(), "gateway".to_string());
        let options = crate::types::StreamOptions {
            base: crate::types::ProviderRequestOptions {
                env: Some(env),
                ..Default::default()
            },
            ..Default::default()
        };
        let stream = wrapped.stream.clone();
        let _ = stream(&model, &ctx, Some(&options));
        let simple = wrapped.stream_simple.clone();
        let _ = simple(&model, &ctx, None);
        let got = captured
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(got.len(), 2);
        // stream: env present -> placeholders materialize.
        assert_eq!(
            got[0],
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai"
        );
        // streamSimple with no options: env unresolvable -> placeholders kept.
        assert!(got[1].contains("{CLOUDFLARE_ACCOUNT_ID}"));
    }

    #[test]
    fn workers_ai_auth_resolves_key_and_account_env() {
        let _guard = crate::utils::env_lock();
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::WorkersAi,
        };
        let ctx = AuthContext::default();
        // Simulate ambient env with an injected AuthContext.
        unsafe {
            std::env::set_var("CLOUDFLARE_API_KEY", "k");
            std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct");
        }
        let check = auth.check(&ctx, None);
        assert!(check.is_some());
        let resolved = auth.resolve(&ctx, None).unwrap();
        assert_eq!(resolved.auth.api_key.as_deref(), Some("k"));
        assert_eq!(
            resolved
                .env
                .as_ref()
                .unwrap()
                .get("CLOUDFLARE_ACCOUNT_ID")
                .map(|s| s.as_str()),
            Some("acct")
        );
        unsafe {
            std::env::remove_var("CLOUDFLARE_API_KEY");
            std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
        }
    }

    #[test]
    fn workers_ai_auth_fails_without_account_id() {
        let _guard = crate::utils::env_lock();
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::WorkersAi,
        };
        let ctx = AuthContext::default();
        unsafe {
            std::env::set_var("CLOUDFLARE_API_KEY", "k");
            std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
        }
        assert!(auth.check(&ctx, None).is_none());
        assert!(auth.resolve(&ctx, None).is_none());
        unsafe {
            std::env::remove_var("CLOUDFLARE_API_KEY");
        }
    }

    #[test]
    fn ai_gateway_auth_sets_cf_aig_authorization_headers() {
        let _guard = crate::utils::env_lock();
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::AiGateway,
        };
        let ctx = AuthContext::default();
        unsafe {
            std::env::set_var("CLOUDFLARE_API_KEY", "k");
            std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct");
            std::env::set_var("CLOUDFLARE_GATEWAY_ID", "gw");
        }
        let check = auth.check(&ctx, None);
        assert!(check.is_some());
        let resolved = auth.resolve(&ctx, None).unwrap();
        assert_eq!(resolved.auth.api_key, None);
        let headers = resolved.auth.headers.unwrap();
        assert_eq!(
            headers
                .get("cf-aig-authorization")
                .and_then(|v| v.as_deref()),
            Some("Bearer k")
        );
        assert_eq!(headers.get("Authorization"), Some(&None));
        assert_eq!(headers.get("x-api-key"), Some(&None));
        assert_eq!(
            resolved
                .env
                .as_ref()
                .unwrap()
                .get("CLOUDFLARE_GATEWAY_ID")
                .map(|s| s.as_str()),
            Some("gw")
        );
        unsafe {
            std::env::remove_var("CLOUDFLARE_API_KEY");
            std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
            std::env::remove_var("CLOUDFLARE_GATEWAY_ID");
        }
    }

    #[test]
    fn stored_credential_wins_over_ambient_env() {
        let _guard = crate::utils::env_lock();
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::WorkersAi,
        };
        let ctx = AuthContext::default();
        unsafe {
            std::env::set_var("CLOUDFLARE_API_KEY", "ambient");
        }
        let cred = ApiKeyCredential {
            key: Some("stored".to_string()),
            env: Some({
                let mut e = ProviderEnv::new();
                e.insert("CLOUDFLARE_ACCOUNT_ID".to_string(), "acct".to_string());
                e
            }),
        };
        let resolved = auth.resolve(&ctx, Some(&cred)).unwrap();
        assert_eq!(resolved.auth.api_key.as_deref(), Some("stored"));
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));
        unsafe {
            std::env::remove_var("CLOUDFLARE_API_KEY");
        }
    }
    fn gateway_binding_options() -> GatewayBindingFetchOptions {
        GatewayBindingFetchOptions {
            base_url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway".to_string(),
            gateway: "my-gateway".to_string(),
        }
    }

    #[test]
    fn cloudflare_binding_translates_provider_endpoint_query_and_json_body() {
        let options = gateway_binding_options();
        let headers = BTreeMap::from([("Anthropic-Version".to_string(), "2023-06-01".to_string())]);
        let request = GatewayBindingFetchRequest {
            method: "post",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/v1/messages?beta=true",
            headers: &headers,
            body: Some(br#"{"model":"claude"}"#),
            signal: None,
        };
        let bare_query = GatewayBindingFetchRequest {
            method: "POST",
            url:
                "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/v1/messages?",
            headers: &headers,
            body: Some(br#"{"model":"claude"}"#),
            signal: None,
        };

        let translated = create_gateway_binding_request(&options, &request).unwrap();
        let bare_translated = create_gateway_binding_request(&options, &bare_query).unwrap();

        assert_eq!(translated.provider, "anthropic");
        assert_eq!(translated.endpoint, "v1/messages?beta=true");
        assert_eq!(translated.query, serde_json::json!({"model": "claude"}));
        assert_eq!(
            translated.headers.get("anthropic-version"),
            Some(&"2023-06-01".to_string())
        );
        assert_eq!(bare_translated.endpoint, "v1/messages");
    }

    #[test]
    fn cloudflare_binding_lowercases_and_strips_derived_headers() {
        let options = gateway_binding_options();
        let headers = BTreeMap::from([
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), "17".to_string()),
            (
                "CF-AIG-Authorization".to_string(),
                format!("Bearer {CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}"),
            ),
            ("Host".to_string(), "gateway.ai.cloudflare.com".to_string()),
            (
                "cf-aig-metadata".to_string(),
                r#"{"user":"42"}"#.to_string(),
            ),
            ("X-API-Key".to_string(), "provider-key".to_string()),
        ]);
        let request = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };

        let translated = create_gateway_binding_request(&options, &request).unwrap();

        assert_eq!(
            translated.headers.get("content-type"),
            Some(&"application/json".to_string())
        );
        assert_eq!(
            translated.headers.get("cf-aig-metadata"),
            Some(&r#"{"user":"42"}"#.to_string())
        );
        assert_eq!(
            translated.headers.get("x-api-key"),
            Some(&"provider-key".to_string())
        );
        assert!(!translated.headers.contains_key("content-length"));
        assert!(!translated.headers.contains_key("host"));
        assert!(!translated.headers.contains_key("cf-aig-authorization"));
    }

    #[test]
    fn cloudflare_binding_normalizes_dot_segments_before_prefix_split() {
        let options = gateway_binding_options();
        let headers = BTreeMap::new();
        let request = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/../anthropic/v1/./messages",
            headers: &headers,
            body: Some(br#"{"model":"claude"}"#),
            signal: None,
        };
        let encoded_dot = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/v1/%2e/messages",
            headers: &headers,
            body: Some(br#"{"model":"claude"}"#),
            signal: None,
        };

        let translated = create_gateway_binding_request(&options, &request).unwrap();
        let encoded_translated = create_gateway_binding_request(&options, &encoded_dot).unwrap();

        assert_eq!(translated.provider, "anthropic");
        assert_eq!(translated.endpoint, "v1/messages");
        assert_eq!(encoded_translated.provider, "anthropic");
        assert_eq!(encoded_translated.endpoint, "v1/messages");
    }

    #[test]
    fn cloudflare_binding_rejects_outside_and_unexpressible_requests() {
        let options = gateway_binding_options();
        let headers = BTreeMap::new();
        let outside = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://api.openai.com/v1/chat/completions",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };
        let encoded_parent = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/%2e%2e/other/anthropic/v1/messages",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };
        let doubled_slash = GatewayBindingFetchRequest {
            method: "POST",
            url:
                "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway//anthropic/v1/messages",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };
        let wrong_method = GatewayBindingFetchRequest {
            method: "GET",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };
        let missing_path = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai",
            headers: &headers,
            body: Some(br#"{}"#),
            signal: None,
        };
        let missing_body = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            headers: &headers,
            body: None,
            signal: None,
        };
        let invalid_body = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            headers: &headers,
            body: Some(b"not json"),
            signal: None,
        };

        assert!(create_gateway_binding_request(&options, &outside)
            .unwrap_err()
            .contains("outside the configured gateway prefix"));
        assert!(create_gateway_binding_request(&options, &encoded_parent)
            .unwrap_err()
            .contains("outside the configured gateway prefix"));
        assert!(create_gateway_binding_request(&options, &doubled_slash)
            .unwrap_err()
            .contains("missing provider/endpoint path"));
        assert!(create_gateway_binding_request(&options, &wrong_method)
            .unwrap_err()
            .contains("cannot express GET"));
        assert!(create_gateway_binding_request(&options, &missing_path)
            .unwrap_err()
            .contains("missing provider/endpoint path"));
        assert!(create_gateway_binding_request(&options, &missing_body)
            .unwrap_err()
            .contains("missing body"));
        assert!(create_gateway_binding_request(&options, &invalid_body)
            .unwrap_err()
            .contains("non-JSON body"));
    }

    type CapturedBindingRequest = (String, AiGatewayUniversalRequest, Option<Arc<AtomicBool>>);
    struct RecordingGatewayBinding {
        captured: Arc<Mutex<Option<CapturedBindingRequest>>>,
    }

    #[async_trait::async_trait]
    impl AiGatewayBinding for RecordingGatewayBinding {
        type Response = String;

        async fn run(
            &self,
            gateway: &str,
            request: AiGatewayUniversalRequest,
            signal: Option<Arc<AtomicBool>>,
        ) -> Result<Self::Response, String> {
            self.captured
                .lock()
                .await
                .replace((gateway.to_string(), request, signal));
            Ok("binding-response".to_string())
        }
    }

    #[tokio::test]
    async fn cloudflare_binding_routes_translated_request_to_gateway() {
        let captured = Arc::new(Mutex::new(None));
        let binding = RecordingGatewayBinding {
            captured: captured.clone(),
        };
        let options = gateway_binding_options();
        let headers = BTreeMap::new();
        let signal = Arc::new(AtomicBool::new(false));
        let request = GatewayBindingFetchRequest {
            method: "POST",
            url: "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/workers-ai/v1/chat/completions",
            headers: &headers,
            body: Some(br#"{"model":"@cf/model"}"#),
            signal: Some(signal.clone()),
        };

        let response = run_gateway_binding_request(&binding, &options, &request)
            .await
            .unwrap();

        assert_eq!(response, "binding-response");
        let (gateway, translated, received_signal) = captured.lock().await.take().unwrap();
        assert_eq!(gateway, "my-gateway");
        assert_eq!(translated.provider, "workers-ai");
        assert_eq!(translated.endpoint, "v1/chat/completions");
        assert_eq!(translated.query, serde_json::json!({"model": "@cf/model"}));
        let received_signal = received_signal.expect("binding should receive the request signal");
        assert!(Arc::ptr_eq(&received_signal, &signal));
    }

    #[test]
    fn cloudflare_provider_auth_preserves_per_field_credential_precedence() {
        let _guard = crate::utils::env_lock();
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::AiGateway,
        };
        let ctx = AuthContext::default();
        unsafe {
            std::env::set_var(CLOUDFLARE_API_KEY, "ambient-key");
            std::env::set_var(CLOUDFLARE_ACCOUNT_ID, "ambient-account");
            std::env::set_var(CLOUDFLARE_GATEWAY_ID, "ambient-gateway");
        }
        let cred = ApiKeyCredential {
            key: Some("stored-key".to_string()),
            env: Some({
                let mut env = ProviderEnv::new();
                env.insert(
                    CLOUDFLARE_ACCOUNT_ID.to_string(),
                    "stored-account".to_string(),
                );
                env
            }),
        };

        let resolved = auth.resolve(&ctx, Some(&cred)).unwrap();

        assert_eq!(
            resolved
                .auth
                .headers
                .as_ref()
                .unwrap()
                .get("cf-aig-authorization"),
            Some(&Some("Bearer stored-key".to_string()))
        );
        assert_eq!(
            resolved.env.as_ref().unwrap().get(CLOUDFLARE_ACCOUNT_ID),
            Some(&"stored-account".to_string())
        );
        assert_eq!(
            resolved.env.as_ref().unwrap().get(CLOUDFLARE_GATEWAY_ID),
            Some(&"ambient-gateway".to_string())
        );
        unsafe {
            std::env::remove_var(CLOUDFLARE_API_KEY);
            std::env::remove_var(CLOUDFLARE_ACCOUNT_ID);
            std::env::remove_var(CLOUDFLARE_GATEWAY_ID);
        }
    }

    #[test]
    fn cloudflare_provider_empty_credential_fields_block_ambient_fallback() {
        let _guard = crate::utils::env_lock();
        let ctx = AuthContext::default();
        unsafe {
            std::env::set_var(CLOUDFLARE_API_KEY, "ambient-key");
            std::env::set_var(CLOUDFLARE_ACCOUNT_ID, "ambient-account");
            std::env::set_var(CLOUDFLARE_GATEWAY_ID, "ambient-gateway");
        }
        let workers_auth = CloudflareAuth {
            kind: CloudflareAuthKind::WorkersAi,
        };
        let empty_key = ApiKeyCredential {
            key: Some(String::new()),
            env: Some({
                let mut env = ProviderEnv::new();
                env.insert(
                    CLOUDFLARE_ACCOUNT_ID.to_string(),
                    "stored-account".to_string(),
                );
                env
            }),
        };
        assert!(workers_auth.resolve(&ctx, Some(&empty_key)).is_none());

        let gateway_auth = CloudflareAuth {
            kind: CloudflareAuthKind::AiGateway,
        };
        let empty_gateway = ApiKeyCredential {
            key: Some("stored-key".to_string()),
            env: Some({
                let mut env = ProviderEnv::new();
                env.insert(
                    CLOUDFLARE_ACCOUNT_ID.to_string(),
                    "stored-account".to_string(),
                );
                env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), String::new());
                env
            }),
        };
        assert!(gateway_auth.resolve(&ctx, Some(&empty_gateway)).is_none());

        unsafe {
            std::env::remove_var(CLOUDFLARE_API_KEY);
            std::env::remove_var(CLOUDFLARE_ACCOUNT_ID);
            std::env::remove_var(CLOUDFLARE_GATEWAY_ID);
        }
    }

    #[test]
    fn cloudflare_provider_headers_keep_inline_upstream_authorization() {
        let auth = CloudflareAuth {
            kind: CloudflareAuthKind::AiGateway,
        };
        let ctx = AuthContext {
            env: Arc::new(|name| match name {
                CLOUDFLARE_API_KEY => Some("gateway-key".to_string()),
                CLOUDFLARE_ACCOUNT_ID => Some("account".to_string()),
                CLOUDFLARE_GATEWAY_ID => Some("gateway".to_string()),
                _ => None,
            }),
            file_exists: Arc::new(|_| false),
        };
        let resolved = auth.resolve(&ctx, None).unwrap();
        let inline = ProviderHeaders::from([(
            "Authorization".to_string(),
            Some("Bearer upstream-token".to_string()),
        )]);
        let merged =
            crate::models::merge_headers(resolved.auth.headers.as_ref(), Some(&inline)).unwrap();

        assert_eq!(
            merged.get("Authorization"),
            Some(&Some("Bearer upstream-token".to_string()))
        );
        assert_eq!(
            merged.get("cf-aig-authorization"),
            Some(&Some("Bearer gateway-key".to_string()))
        );
        assert_eq!(merged.get("x-api-key"), Some(&None));
    }

    #[test]
    fn cloudflare_provider_base_url_uses_scoped_account_and_gateway_env() {
        let mut env = ProviderEnv::new();
        env.insert(
            CLOUDFLARE_ACCOUNT_ID.to_string(),
            "request-account".to_string(),
        );
        env.insert(
            CLOUDFLARE_GATEWAY_ID.to_string(),
            "request-gateway".to_string(),
        );

        let resolved = resolve_cloudflare_model(&cloudflare_model(), Some(&env));

        assert_eq!(
            resolved.base_url,
            "https://gateway.ai.cloudflare.com/v1/request-account/request-gateway/openai"
        );
    }
}
