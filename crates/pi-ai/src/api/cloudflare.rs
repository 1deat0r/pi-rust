//! Cloudflare base URLs + provider stream wrapping — port of
//! `packages/ai/src/api/cloudflare.ts` and the HTTPS-facing parts of
//! `packages/ai/src/providers/cloudflare-stream.ts` and
//! `cloudflare-auth.ts`.
//!
//! Cloudflare's two providers reuse the standard API adaptors (anthropic-
//! messages / openai-completions / openai-responses) against gateway /
//! Workers-AI endpoints whose URLs contain `{CLOUDFLARE_ACCOUNT_ID}` /
//! `{CLOUDFLARE_GATEWAY_ID}` placeholders; the placeholders materialize from
//! the resolved provider env before dispatch. Auth accepts a stored
//! credential or ambient `CLOUDFLARE_API_KEY` plus account (and gateway)
//! ids, per-field merged.
//!
//! DOCUMENTED DIVERGENCE: upstream also ships `cloudflare-gateway-binding.ts`,
//! a transport that routes HTTPS gateway URLs through the Workers AI binding
//! (`env.AI.gateway(...)`) inside a Cloudflare Worker. That surface only
//! exists in the Workers runtime and has no Rust analog here; the HTTPS wire
//! behavior this port implements is identical for anything outside a Worker.
//! The `TODO` marker below is the seam where binding-routing would plug in.

use std::sync::Arc;

use crate::auth::{ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthResult, ModelAuth, ProviderAuth};
use crate::model::Model;
use crate::models::ProviderStreams;
use crate::types::{ProviderEnv, ProviderHeaders};

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

/// Per-field merge: prefer the credential value, fall back to ambient env
/// (upstream `resolveValue`).
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
        Some(v) if !v.is_empty() => Some(v),
        _ => get_env_value(name, None, ctx),
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
        let api_key = resolve_value(CLOUDFLARE_API_KEY, ctx, credential)?;
        let account_id = resolve_value(CLOUDFLARE_ACCOUNT_ID, ctx, credential)?;
        let gateway_id = match self.kind {
            CloudflareAuthKind::WorkersAi => None,
            CloudflareAuthKind::AiGateway => Some(resolve_value(CLOUDFLARE_GATEWAY_ID, ctx, credential)?),
        };
        if api_key.is_empty() || account_id.is_empty() {
            return None;
        }
        let mut env = ProviderEnv::new();
        env.insert(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id);
        if let Some(gateway_id) = gateway_id {
            env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
        }
        Some((api_key, env, if credential.is_some() { "stored credential".to_string() } else { CLOUDFLARE_API_KEY.to_string() }))
    }
}

impl ApiKeyAuth for CloudflareAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
    }

    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        self.resolved(ctx, credential).map(|(_, _, source)| AuthCheck {
            source: Some(source),
            auth_type: "api_key",
        })
    }

    fn resolve(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        let (api_key, env, source) = self.resolved(ctx, credential)?;
        let auth = match self.kind {
            CloudflareAuthKind::WorkersAi => ModelAuth {
                api_key: Some(api_key),
                headers: None,
                base_url: None,
            },
            CloudflareAuthKind::AiGateway => {
                let mut headers = ProviderHeaders::new();
                headers.insert("cf-aig-authorization".to_string(), Some(format!("Bearer {api_key}")));
                headers.insert("Authorization".to_string(), None);
                headers.insert("x-api-key".to_string(), None);
                ModelAuth { api_key: None, headers: Some(headers), base_url: None }
            }
        };
        Some(AuthResult { auth, env: Some(env), source: Some(source) })
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
// NOTE: the Workers AI binding transport seam (`createGatewayBindingFetch`)
// would live here; see the module-level DOCUMENTED DIVERGENCE.
pub fn cloudflare_streams(inner: ProviderStreams) -> ProviderStreams {
    let stream: crate::models::StreamFn = {
        let inner = inner.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| -> crate::event_stream::AssistantMessageEventStream {
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
                  options: Option<&crate::types::SimpleStreamOptions>| -> crate::event_stream::AssistantMessageEventStream {
                let env = options.and_then(|o| o.base.base.env.as_ref());
                let resolved = resolve_cloudflare_model(model, env);
                let f = inner.stream_simple.clone();
                f(&resolved, ctx, options)
            },
        )
    };
    ProviderStreams { stream, stream_simple, fetch_deferred: None, cancel_deferred: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::AssistantMessageEventStream;

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
                  _o: Option<&crate::types::StreamOptions>| -> AssistantMessageEventStream {
                captured_for_stream.lock().unwrap().push(model.base_url.clone());
                AssistantMessageEventStream::new()
            },
        );
        let simple_fn: crate::models::SimpleStreamFn = Arc::new(
            move |model: &Model,
                  _ctx: &crate::types::Context,
                  _o: Option<&crate::types::SimpleStreamOptions>| -> AssistantMessageEventStream {
                captured_for_simple.lock().unwrap().push(model.base_url.clone());
                AssistantMessageEventStream::new()
            },
        );
        let inner = ProviderStreams { stream: stream_fn, stream_simple: simple_fn, fetch_deferred: None, cancel_deferred: None };
        let wrapped = cloudflare_streams(inner);
        let model = cloudflare_model();
        let ctx = crate::types::Context::default();
        let mut env = ProviderEnv::new();
        env.insert("CLOUDFLARE_ACCOUNT_ID".to_string(), "account".to_string());
        env.insert("CLOUDFLARE_GATEWAY_ID".to_string(), "gateway".to_string());
        let options = crate::types::StreamOptions {
            base: crate::types::ProviderRequestOptions { env: Some(env), ..Default::default() },
            ..Default::default()
        };
        let stream = wrapped.stream.clone();
        let _ = stream(&model, &ctx, Some(&options));
        let simple = wrapped.stream_simple.clone();
        let _ = simple(&model, &ctx, None);
        let got = captured.lock().unwrap().clone();
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
        let auth = CloudflareAuth { kind: CloudflareAuthKind::WorkersAi };
        let ctx = AuthContext::default();
        // Simulate ambient env with an injected AuthContext.
        unsafe { std::env::set_var("CLOUDFLARE_API_KEY", "k"); std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct"); }
        let check = auth.check(&ctx, None);
        assert!(check.is_some());
        let resolved = auth.resolve(&ctx, None).unwrap();
        assert_eq!(resolved.auth.api_key.as_deref(), Some("k"));
        assert_eq!(resolved.env.as_ref().unwrap().get("CLOUDFLARE_ACCOUNT_ID").map(|s| s.as_str()), Some("acct"));
        unsafe { std::env::remove_var("CLOUDFLARE_API_KEY"); std::env::remove_var("CLOUDFLARE_ACCOUNT_ID"); }
    }

    #[test]
    fn workers_ai_auth_fails_without_account_id() {
        let auth = CloudflareAuth { kind: CloudflareAuthKind::WorkersAi };
        let ctx = AuthContext::default();
        unsafe { std::env::set_var("CLOUDFLARE_API_KEY", "k"); std::env::remove_var("CLOUDFLARE_ACCOUNT_ID"); }
        assert!(auth.check(&ctx, None).is_none());
        assert!(auth.resolve(&ctx, None).is_none());
        unsafe { std::env::remove_var("CLOUDFLARE_API_KEY"); }
    }

    #[test]
    fn ai_gateway_auth_sets_cf_aig_authorization_headers() {
        let auth = CloudflareAuth { kind: CloudflareAuthKind::AiGateway };
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
        assert_eq!(headers.get("cf-aig-authorization").and_then(|v| v.as_deref()), Some("Bearer k"));
        assert_eq!(headers.get("Authorization"), Some(&None));
        assert_eq!(headers.get("x-api-key"), Some(&None));
        assert_eq!(resolved.env.as_ref().unwrap().get("CLOUDFLARE_GATEWAY_ID").map(|s| s.as_str()), Some("gw"));
        unsafe {
            std::env::remove_var("CLOUDFLARE_API_KEY");
            std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
            std::env::remove_var("CLOUDFLARE_GATEWAY_ID");
        }
    }

    #[test]
    fn stored_credential_wins_over_ambient_env() {
        let auth = CloudflareAuth { kind: CloudflareAuthKind::WorkersAi };
        let ctx = AuthContext::default();
        unsafe { std::env::set_var("CLOUDFLARE_API_KEY", "ambient"); }
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
        unsafe { std::env::remove_var("CLOUDFLARE_API_KEY"); }
    }
}
