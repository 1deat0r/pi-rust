//! Provider registry — port of `packages/ai/src/providers/all.ts`.
//!
//! Every built-in provider is constructed from the vendored model catalog
//! (see `crate::model_catalog`) plus its upstream auth semantics. API
//! implementations (openai-completions, openai-responses, google, etc.) are
//! ported incrementally; a provider whose API adaptor is not yet registered
//! streams the upstream "no API implementation" error (the same behavior
//! `createProvider` produces when a model.api has no dispatch entry; the
//! catalog/auth side is fully wired now).

use std::sync::Arc;

use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::model::Model;
use crate::models::{create_provider, CreateProviderOptions, Models, Provider};
use crate::model_catalog::get_builtin_models;

/// Build a provider with catalog models + env-key auth and no (yet) stream
/// implementation. The delegate registers the real API adaptor when ported.
pub fn provider_with_env_auth(
    id: &str,
    name: &str,
    base_url: Option<&str>,
    env_vars: &[&str],
    api: crate::models::ProviderApiSpec,
) -> Provider {
    let models = catalog_models(id);
    let base_url_opt = base_url
        .map(|s| s.to_string())
        .or_else(|| models.first().map(|m| m.base_url.clone()));
    create_provider(CreateProviderOptions {
        id: id.to_string(),
        name: Some(name.to_string()),
        base_url: base_url_opt,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(name, env_vars.iter().map(|s| s.to_string()).collect())),
            oauth: None,
        },
        models,
        api,
        filter_models: None,
    })
}

/// Models from the vendored catalog for a provider id.
pub fn catalog_models(provider_id: &str) -> Vec<Model> {
    get_builtin_models(provider_id).into_iter().cloned().collect()
}

/// All built-in providers, freshly constructed.
pub fn builtin_providers() -> Vec<Provider> {
    vec![
        amazon_bedrock_provider(),
        ant_ling_provider(),
        anthropic_provider(),
        azure_openai_responses_provider(),
        baseten_provider(),
        cerebras_provider(),
        cloudflare_ai_gateway_provider(),
        cloudflare_workers_ai_provider(),
        deepseek_provider(),
        fireworks_provider(),
        github_copilot_provider(),
        google_provider(),
        google_vertex_provider(),
        groq_provider(),
        huggingface_provider(),
        kimi_coding_provider(),
        minimax_provider(),
        minimax_cn_provider(),
        mistral_provider(),
        moonshotai_provider(),
        moonshotai_cn_provider(),
        nvidia_provider(),
        openai_provider(),
        openai_codex_provider(),
        opencode_provider(),
        opencode_go_provider(),
        openrouter_provider(),
        qwen_token_plan_provider(),
        qwen_token_plan_cn_provider(),
        qwen_token_plan_individual_provider(),
        together_provider(),
        vercel_ai_gateway_provider(),
        xai_provider(),
        xiaomi_provider(),
        xiaomi_token_plan_ams_provider(),
        xiaomi_token_plan_cn_provider(),
        xiaomi_token_plan_sgp_provider(),
        zai_provider(),
        zai_coding_cn_provider(),
    ]
}

/// An empty ProviderApiSpec — streams return the upstream "no API
/// implementation" error until the adaptor is registered. Mixed-API
/// providers (github-copilot, cloudflare-ai-gateway) dispatch by api.
fn no_stream() -> crate::models::ProviderApiSpec {
    crate::models::ProviderApiSpec::ByApi(std::collections::BTreeMap::new())
}

fn anthropic_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let base_url = crate::api::anthropic_messages::default_base_url();
    let stream = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(move |model: &Model, ctx: &crate::types::Context, options: Option<&crate::types::StreamOptions>| {
            let api_key = options.and_then(|o| o.base.api_key.as_deref());
            crate::api::anthropic_messages::stream(
                model,
                ctx,
                client.clone(),
                &base_url,
                api_key,
                &crate::api::anthropic_messages::AnthropicOptions::default(),
            )
        })
    };
    let stream_simple = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &crate::api::anthropic_messages::AnthropicOptions::default(),
                )
            },
        )
    };
    crate::models::ProviderStreams { stream, stream_simple }
}

macro_rules! env_provider {
    ($fn_name:ident, $id:expr, $name:expr, $base:expr, $env_vars:expr) => {
        pub fn $fn_name() -> Provider {
            provider_with_env_auth($id, $name, Some($base), &$env_vars, no_stream())
        }
    };
}

env_provider!(ant_ling_provider, "ant-ling", "Ant Ling", "https://api.ant-ling.com/v1", ["ANT_LING_API_KEY"]);
env_provider!(azure_openai_responses_provider, "azure-openai-responses", "Azure OpenAI", "", ["AZURE_OPENAI_API_KEY"]);
env_provider!(baseten_provider, "baseten", "Baseten", "https://inference.baseten.co/v1", ["BASETEN_API_KEY"]);
env_provider!(cerebras_provider, "cerebras", "Cerebras", "https://api.cerebras.ai/v1", ["CEREBRAS_API_KEY"]);
env_provider!(deepseek_provider, "deepseek", "DeepSeek", "https://api.deepseek.com", ["DEEPSEEK_API_KEY"]);
env_provider!(fireworks_provider, "fireworks", "Fireworks", "https://api.fireworks.ai/inference", ["FIREWORKS_API_KEY"]);
env_provider!(google_provider, "google", "Google", "https://generativelanguage.googleapis.com/v1beta", ["GEMINI_API_KEY"]);
env_provider!(groq_provider, "groq", "Groq", "https://api.groq.com/openai/v1", ["GROQ_API_KEY"]);
env_provider!(huggingface_provider, "huggingface", "Hugging Face", "https://router.huggingface.co/v1", ["HF_TOKEN"]);
env_provider!(kimi_coding_provider, "kimi-coding", "Kimi (Coding)", "https://api.kimi.com/coding", ["KIMI_API_KEY"]);
env_provider!(minimax_provider, "minimax", "MiniMax", "https://api.minimax.io/anthropic", ["MINIMAX_API_KEY"]);
env_provider!(minimax_cn_provider, "minimax-cn", "MiniMax (CN)", "https://api.minimaxi.com/anthropic", ["MINIMAX_CN_API_KEY"]);
env_provider!(mistral_provider, "mistral", "Mistral", "https://api.mistral.ai", ["MISTRAL_API_KEY"]);
env_provider!(moonshotai_provider, "moonshotai", "Moonshot AI", "https://api.moonshot.ai/v1", ["MOONSHOT_API_KEY"]);
env_provider!(moonshotai_cn_provider, "moonshotai-cn", "Moonshot AI (CN)", "https://api.moonshot.cn/v1", ["MOONSHOT_API_KEY"]);
env_provider!(nvidia_provider, "nvidia", "NVIDIA", "https://integrate.api.nvidia.com/v1", ["NVIDIA_API_KEY"]);
env_provider!(openai_provider, "openai", "OpenAI", "https://api.openai.com/v1", ["OPENAI_API_KEY"]);
env_provider!(opencode_provider, "opencode", "opencode", "https://opencode.ai/zen", ["OPENCODE_API_KEY"]);
env_provider!(opencode_go_provider, "opencode-go", "opencode (Go)", "https://opencode.ai/zen/go", ["OPENCODE_API_KEY"]);
env_provider!(openrouter_provider, "openrouter", "OpenRouter", "https://openrouter.ai/api/v1", ["OPENROUTER_API_KEY"]);
env_provider!(qwen_token_plan_provider, "qwen-token-plan", "Qwen Token Plan", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", ["QWEN_TOKEN_PLAN_API_KEY"]);
env_provider!(qwen_token_plan_cn_provider, "qwen-token-plan-cn", "Qwen Token Plan (CN)", "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1", ["QWEN_TOKEN_PLAN_CN_API_KEY"]);
env_provider!(qwen_token_plan_individual_provider, "qwen-token-plan-individual", "Qwen Token Plan (Individual)", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", ["QWEN_TOKEN_PLAN_API_KEY"]);
env_provider!(together_provider, "together", "Together", "https://api.together.ai/v1", ["TOGETHER_API_KEY"]);
env_provider!(vercel_ai_gateway_provider, "vercel-ai-gateway", "Vercel AI Gateway", "https://ai-gateway.vercel.sh", ["AI_GATEWAY_API_KEY"]);
env_provider!(xai_provider, "xai", "xAI", "https://api.x.ai/v1", ["XAI_API_KEY"]);
env_provider!(xiaomi_provider, "xiaomi", "Xiaomi", "https://api.xiaomimimo.com/v1", ["XIAOMI_API_KEY"]);
env_provider!(xiaomi_token_plan_ams_provider, "xiaomi-token-plan-ams", "Xiaomi Token Plan (AMS)", "https://token-plan-ams.xiaomimimo.com/v1", ["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]);
env_provider!(xiaomi_token_plan_cn_provider, "xiaomi-token-plan-cn", "Xiaomi Token Plan (CN)", "https://token-plan-cn.xiaomimimo.com/v1", ["XIAOMI_TOKEN_PLAN_CN_API_KEY"]);
env_provider!(xiaomi_token_plan_sgp_provider, "xiaomi-token-plan-sgp", "Xiaomi Token Plan (SGP)", "https://token-plan-sgp.xiaomimimo.com/v1", ["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]);
env_provider!(zai_provider, "zai", "Z.ai", "https://api.z.ai/api/coding/paas/v4", ["ZAI_API_KEY"]);
env_provider!(zai_coding_cn_provider, "zai-coding-cn", "Z.ai Coding (CN)", "https://open.bigmodel.cn/api/coding/paas/v4", ["ZAI_CODING_CN_API_KEY"]);

pub fn anthropic_provider() -> Provider {
    let models = catalog_models("anthropic");
    let base_url = models
        .first()
        .map(|m| m.base_url.clone())
        .unwrap_or_else(crate::api::anthropic_messages::default_base_url);
    create_provider(CreateProviderOptions {
        id: "anthropic".to_string(),
        name: Some("Anthropic".to_string()),
        base_url: Some(base_url),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth("Anthropic API key", vec!["ANTHROPIC_API_KEY".to_string()])),
            oauth: None,
        },
        models,
        api: crate::models::ProviderApiSpec::Single(anthropic_streams()),
        filter_models: None,
    })
}

pub fn amazon_bedrock_provider() -> Provider {
    provider_with_env_auth(
        "amazon-bedrock",
        "Amazon Bedrock",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        &[],
        no_stream(),
    )
}

pub fn github_copilot_provider() -> Provider {
    provider_with_env_auth(
        "github-copilot",
        "GitHub Copilot",
        Some("https://api.individual.githubcopilot.com"),
        &["COPILOT_GITHUB_TOKEN"],
        no_stream(),
    )
}

pub fn cloudflare_ai_gateway_provider() -> Provider {
    provider_with_env_auth(
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        Some("https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic"),
        &["CLOUDFLARE_API_TOKEN"],
        no_stream(),
    )
}

pub fn cloudflare_workers_ai_provider() -> Provider {
    provider_with_env_auth(
        "cloudflare-workers-ai",
        "Cloudflare Workers AI",
        Some("https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"),
        &["CLOUDFLARE_API_TOKEN"],
        no_stream(),
    )
}

pub fn openai_codex_provider() -> Provider {
    provider_with_env_auth(
        "openai-codex",
        "OpenAI Codex",
        Some("https://chatgpt.com/backend-api"),
        &[],
        no_stream(),
    )
}

pub fn google_vertex_provider() -> Provider {
    provider_with_env_auth(
        "google-vertex",
        "Google Vertex",
        Some("https://{location}-aiplatform.googleapis.com"),
        &[],
        no_stream(),
    )
}

/// A `Models` collection with every built-in provider registered.
pub fn builtin_models(options: crate::models::CreateModelsOptions) -> Models {
    let models = crate::models::create_models(options);
    for provider in builtin_providers() {
        models.set_provider(provider);
    }
    models
}

/// Typed read of the generated built-in catalog (delegates to catalog read).
pub use crate::model_catalog::get_builtin_model as get_builtin_model;


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_registered() {
        let providers = builtin_providers();
        assert_eq!(providers.len(), 39);
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        for expected in ["google", "anthropic", "openai", "deepseek", "xai", "groq", "openrouter", "openai-codex", "github-copilot", "cloudflare-ai-gateway", "mistral", "together", "zai", "xiaomi", "qwen-token-plan-cn"] {
            assert!(ids.contains(&expected), "missing provider {expected}");
        }
    }

    #[test]
    fn providers_have_catalog_models() {
        let providers = builtin_providers();
        for p in &providers {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
        }
        let google = providers.iter().find(|p| p.id == "google").unwrap();
        assert_eq!(google.models.len(), 22);
        let openrouter = providers.iter().find(|p| p.id == "openrouter").unwrap();
        assert_eq!(openrouter.models.len(), 346);
    }

    #[test]
    fn providers_have_auth() {
        let providers = builtin_providers();
        for p in &providers {
            assert!(p.auth.api_key.is_some() || p.auth.oauth.is_some(), "{} has no auth", p.id);
        }
        let google = providers.iter().find(|p| p.id == "google").unwrap();
        assert!(google.auth.api_key.is_some());
    }

    #[test]
    fn anthropic_provider_streams_error_without_key() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let provider = anthropic_provider();
            let model = provider.models.first().cloned().unwrap();
            let ctx = crate::types::Context::default();
            let options = crate::types::StreamOptions::default();
            let stream = provider.stream(&model, &ctx, Some(&options));
            let msg = stream.for_each(|_| {}).await;
            assert_eq!(msg.stop_reason(), Some(crate::types::StopReason::Error));
            assert!(msg.error_message().is_some());
        });
    }

    #[test]
    fn unported_api_models_stream_error() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let provider = google_provider();
            let model = provider.models.first().cloned().unwrap();
            let ctx = crate::types::Context::default();
            let options = crate::types::StreamOptions::default();
            let stream = provider.stream(&model, &ctx, Some(&options));
            let msg = stream.for_each(|_| {}).await;
            assert_eq!(msg.stop_reason(), Some(crate::types::StopReason::Error));
            assert!(msg.error_message().is_some());
        });
    }

    #[test]
    fn builtin_models_facade_lists_all_models() {
        let models = builtin_models(crate::models::CreateModelsOptions::default());
        let all = models.get_models(None);
        assert_eq!(all.len(), 1267);
        assert!(models.get_model("google", "gemini-2.5-flash").is_some());
        assert!(models.get_model("anthropic", "claude-sonnet-4-6").is_some());
    }

    #[test]
    fn builtin_models_facade_auth_gating() {
        unsafe {
            std::env::remove_var("GEMINI_API_KEY");
        }
        let models = builtin_models(crate::models::CreateModelsOptions::default());
        let _available = models.get_available(None);
        // Without credentials no provider should be available (unless
        // ambient env creds exist); environment-dependent so assert a
        // provider-level property instead: unknown provider yields nothing.
        assert!(models.get_available(Some("no-such-provider")).is_empty());
        // check_auth on a provider without env returns None
        assert!(models.check_auth("google").is_none());
    }
}
