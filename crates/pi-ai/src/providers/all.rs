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
use crate::model_catalog::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Models, Provider};

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
            api_key: Some(env_api_key_auth(
                name,
                env_vars.iter().map(|s| s.to_string()).collect(),
            )),
            oauth: None,
        },
        models,
        api,
        filter_models: None,
    })
}

/// Models from the vendored catalog for a provider id.
pub fn catalog_models(provider_id: &str) -> Vec<Model> {
    get_builtin_models(provider_id)
        .into_iter()
        .cloned()
        .collect()
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

fn anthropic_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let base_url = crate::api::anthropic_messages::default_base_url();
    let stream = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
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
    crate::models::ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// ProviderStreams for the openai-completions API family. Each provider
/// instance owns its reqwest client + base URL; the api key comes from the
/// auth-applied options.
pub fn openai_completions_streams(base_url: String) -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream_base = base_url.clone();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let chat_options = crate::api::openai_completions::OpenAIChatOptions {
                    base: options.cloned().unwrap_or_default(),
                    reasoning_effort: None,
                    tool_choice: None,
                    thinking_budgets: None,
                };
                crate::api::openai_completions::stream(
                    model,
                    ctx,
                    client.clone(),
                    &stream_base,
                    api_key,
                    &chat_options,
                )
            },
        )
    };
    let simple_base = base_url;
    let stream_simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let Some(options) = options else {
                    return crate::event_stream::create_error_stream(
                        &model.api,
                        &model.provider,
                        &model.id,
                        "streamSimple requires options".to_string(),
                    );
                };
                crate::api::openai_completions::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    &simple_base,
                    api_key,
                    options,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

macro_rules! env_provider {
    ($fn_name:ident, $id:expr, $name:expr, $base:expr, $env_vars:expr) => {
        pub fn $fn_name() -> Provider {
            provider_with_env_auth(
                $id,
                $name,
                Some($base),
                &$env_vars,
                crate::models::ProviderApiSpec::Single(openai_completions_streams(
                    $base.to_string(),
                )),
            )
        }
    };
}

env_provider!(
    ant_ling_provider,
    "ant-ling",
    "Ant Ling",
    "https://api.ant-ling.com/v1",
    ["ANT_LING_API_KEY"]
);
pub fn azure_openai_responses_provider() -> Provider {
    let models = catalog_models("azure-openai-responses");
    let base_url = models
        .first()
        .map(|m| m.base_url.clone())
        .unwrap_or_default();
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let go = crate::api::azure_openai_responses::AzureOpenAIResponsesOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::azure_openai_responses::stream(model, ctx, client.clone(), api_key, &go)
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::azure_openai_responses::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    api_key,
                    &opts,
                )
            },
        )
    };
    create_provider(CreateProviderOptions {
        id: "azure-openai-responses".to_string(),
        name: Some("Azure OpenAI".to_string()),
        base_url: Some(base_url),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Azure OpenAI API key",
                vec!["AZURE_OPENAI_API_KEY".to_string()],
            )),
            oauth: None,
        },
        models,
        api: crate::models::ProviderApiSpec::Single(crate::models::ProviderStreams {
            stream,
            stream_simple: simple,
            fetch_deferred: None,
            cancel_deferred: None,
        }),
        filter_models: None,
    })
}
env_provider!(
    baseten_provider,
    "baseten",
    "Baseten",
    "https://inference.baseten.co/v1",
    ["BASETEN_API_KEY"]
);
env_provider!(
    cerebras_provider,
    "cerebras",
    "Cerebras",
    "https://api.cerebras.ai/v1",
    ["CEREBRAS_API_KEY"]
);
env_provider!(
    deepseek_provider,
    "deepseek",
    "DeepSeek",
    "https://api.deepseek.com",
    ["DEEPSEEK_API_KEY"]
);
env_provider!(
    fireworks_provider,
    "fireworks",
    "Fireworks",
    "https://api.fireworks.ai/inference",
    ["FIREWORKS_API_KEY"]
);
pub fn google_provider() -> Provider {
    google_provider_real()
}
env_provider!(
    groq_provider,
    "groq",
    "Groq",
    "https://api.groq.com/openai/v1",
    ["GROQ_API_KEY"]
);
env_provider!(
    huggingface_provider,
    "huggingface",
    "Hugging Face",
    "https://router.huggingface.co/v1",
    ["HF_TOKEN"]
);
env_provider!(
    kimi_coding_provider,
    "kimi-coding",
    "Kimi (Coding)",
    "https://api.kimi.com/coding",
    ["KIMI_API_KEY"]
);
env_provider!(
    minimax_provider,
    "minimax",
    "MiniMax",
    "https://api.minimax.io/anthropic",
    ["MINIMAX_API_KEY"]
);
env_provider!(
    minimax_cn_provider,
    "minimax-cn",
    "MiniMax (CN)",
    "https://api.minimaxi.com/anthropic",
    ["MINIMAX_CN_API_KEY"]
);
pub fn mistral_provider() -> Provider {
    provider_with_env_auth(
        "mistral",
        "Mistral",
        Some("https://api.mistral.ai"),
        &["MISTRAL_API_KEY"],
        crate::models::ProviderApiSpec::Single(mistral_conversations_streams()),
    )
}
env_provider!(
    moonshotai_provider,
    "moonshotai",
    "Moonshot AI",
    "https://api.moonshot.ai/v1",
    ["MOONSHOT_API_KEY"]
);
env_provider!(
    moonshotai_cn_provider,
    "moonshotai-cn",
    "Moonshot AI (CN)",
    "https://api.moonshot.cn/v1",
    ["MOONSHOT_API_KEY"]
);
env_provider!(
    nvidia_provider,
    "nvidia",
    "NVIDIA",
    "https://integrate.api.nvidia.com/v1",
    ["NVIDIA_API_KEY"]
);
pub fn openai_provider() -> Provider {
    let base = "https://api.openai.com/v1";
    provider_with_env_auth(
        "openai",
        "OpenAI",
        Some(base),
        &["OPENAI_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_responses_streams(base.to_string())),
    )
}
pub fn opencode_provider() -> Provider {
    let models = catalog_models("opencode");
    let base_url = models
        .first()
        .map(|m| m.base_url.clone())
        .unwrap_or_default();
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_for(&base_url),
    );
    streams.insert(
        "google-generative-ai".to_string(),
        google_streams(
            base_url.clone(),
            crate::api::google_generative_ai::DEFAULT_BASE_URL,
        ),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams(base_url.clone()),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams(base_url),
    );
    provider_with_env_auth(
        "opencode",
        "opencode",
        Some("https://opencode.ai/zen"),
        &["OPENCODE_API_KEY"],
        crate::models::ProviderApiSpec::ByApi(streams),
    )
}
pub fn opencode_go_provider() -> Provider {
    let models = catalog_models("opencode-go");
    let base_url = models
        .first()
        .map(|m| m.base_url.clone())
        .unwrap_or_default();
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams(base_url.clone()),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams(base_url),
    );
    provider_with_env_auth(
        "opencode-go",
        "opencode (Go)",
        Some("https://opencode.ai/zen/go"),
        &["OPENCODE_API_KEY"],
        crate::models::ProviderApiSpec::ByApi(streams),
    )
}
pub fn openrouter_provider() -> Provider {
    let mut provider = provider_with_env_auth(
        "openrouter",
        "OpenRouter",
        Some("https://openrouter.ai/api/v1"),
        &["OPENROUTER_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_completions_streams(
            "https://openrouter.ai/api/v1".to_string(),
        )),
    );
    provider.auth.oauth = Some(crate::auth_flows::OpenRouterOAuth::new());
    provider
}
env_provider!(
    qwen_token_plan_provider,
    "qwen-token-plan",
    "Qwen Token Plan",
    "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    ["QWEN_TOKEN_PLAN_API_KEY"]
);
env_provider!(
    qwen_token_plan_cn_provider,
    "qwen-token-plan-cn",
    "Qwen Token Plan (CN)",
    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
    ["QWEN_TOKEN_PLAN_CN_API_KEY"]
);
env_provider!(
    qwen_token_plan_individual_provider,
    "qwen-token-plan-individual",
    "Qwen Token Plan (Individual)",
    "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    ["QWEN_TOKEN_PLAN_API_KEY"]
);
env_provider!(
    together_provider,
    "together",
    "Together",
    "https://api.together.ai/v1",
    ["TOGETHER_API_KEY"]
);
pub fn vercel_ai_gateway_provider() -> Provider {
    provider_with_env_auth(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        Some("https://ai-gateway.vercel.sh"),
        &["AI_GATEWAY_API_KEY"],
        crate::models::ProviderApiSpec::Single(anthropic_streams_for(
            "https://ai-gateway.vercel.sh",
        )),
    )
}
env_provider!(
    xai_provider,
    "xai",
    "xAI",
    "https://api.x.ai/v1",
    ["XAI_API_KEY"]
);
env_provider!(
    xiaomi_provider,
    "xiaomi",
    "Xiaomi",
    "https://api.xiaomimimo.com/v1",
    ["XIAOMI_API_KEY"]
);
env_provider!(
    xiaomi_token_plan_ams_provider,
    "xiaomi-token-plan-ams",
    "Xiaomi Token Plan (AMS)",
    "https://token-plan-ams.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]
);
env_provider!(
    xiaomi_token_plan_cn_provider,
    "xiaomi-token-plan-cn",
    "Xiaomi Token Plan (CN)",
    "https://token-plan-cn.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_CN_API_KEY"]
);
env_provider!(
    xiaomi_token_plan_sgp_provider,
    "xiaomi-token-plan-sgp",
    "Xiaomi Token Plan (SGP)",
    "https://token-plan-sgp.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]
);
env_provider!(
    zai_provider,
    "zai",
    "Z.ai",
    "https://api.z.ai/api/coding/paas/v4",
    ["ZAI_API_KEY"]
);
env_provider!(
    zai_coding_cn_provider,
    "zai-coding-cn",
    "Z.ai Coding (CN)",
    "https://open.bigmodel.cn/api/coding/paas/v4",
    ["ZAI_CODING_CN_API_KEY"]
);

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
            api_key: Some(env_api_key_auth(
                "Anthropic API key",
                vec!["ANTHROPIC_API_KEY".to_string()],
            )),
            oauth: Some(crate::auth_flows::AnthropicOAuth::new()),
        },
        models,
        api: crate::models::ProviderApiSpec::Single(anthropic_streams()),
        filter_models: None,
    })
}

pub fn amazon_bedrock_provider() -> Provider {
    create_provider(CreateProviderOptions {
        id: "amazon-bedrock".to_string(),
        name: Some("Amazon Bedrock".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth_with_env_check()),
            oauth: None,
        },
        models: catalog_models("amazon-bedrock"),
        api: crate::models::ProviderApiSpec::Single(bedrock_streams()),
        filter_models: None,
    })
}

/// Bedrock auth accepts a bearer token or ambient AWS credential chain. The
/// resolve/check logic lives in the adaptor (`bedrock_converse::resolve_config`);
/// this auth only needs to report availability when any AWS credential source
/// exists upstream would accept.
fn env_api_key_auth_with_env_check() -> Arc<dyn crate::auth::ApiKeyAuth> {
    struct BedrockAuth;
    impl crate::auth::ApiKeyAuth for BedrockAuth {
        fn name(&self) -> &str {
            "AWS credentials or bearer token"
        }
        fn check(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthCheck> {
            if credential.map(|c| c.key.is_some()).unwrap_or(false) {
                return Some(crate::auth::AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            if credential
                .and_then(|c| c.env.as_ref())
                .is_some_and(|e| e.contains_key("AWS_PROFILE"))
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("AWS_PROFILE".to_string()),
                    auth_type: "api_key",
                });
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.is_empty());
            if env("AWS_BEARER_TOKEN_BEDROCK").is_some()
                || env("AWS_PROFILE").is_some()
                || (env("AWS_ACCESS_KEY_ID").is_some() && env("AWS_SECRET_ACCESS_KEY").is_some())
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("AWS credentials".to_string()),
                    auth_type: "api_key",
                });
            }
            None
        }
        fn resolve(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthResult> {
            if let Some(cred) = credential {
                if cred.key.is_some() {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth {
                            api_key: cred.key.clone(),
                            headers: None,
                            base_url: None,
                        },
                        env: cred.env.clone(),
                        source: Some("stored credential".to_string()),
                    });
                }
                if cred
                    .env
                    .as_ref()
                    .is_some_and(|e| e.contains_key("AWS_PROFILE"))
                {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth::default(),
                        env: cred.env.clone(),
                        source: Some("stored credential".to_string()),
                    });
                }
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.is_empty());
            if let Some(token) = env("AWS_BEARER_TOKEN_BEDROCK") {
                let _ = token;
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("AWS_BEARER_TOKEN_BEDROCK".to_string()),
                });
            }
            if env("AWS_PROFILE").is_some() {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("AWS_PROFILE".to_string()),
                });
            }
            if env("AWS_ACCESS_KEY_ID").is_some() && env("AWS_SECRET_ACCESS_KEY").is_some() {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("AWS access keys".to_string()),
                });
            }
            None
        }
    }
    Arc::new(BedrockAuth)
}

pub fn github_copilot_provider() -> Provider {
    let mut streams = std::collections::BTreeMap::new();
    let base = "https://api.individual.githubcopilot.com";
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_for(base),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams(base.to_string()),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams(base.to_string()),
    );
    create_provider(CreateProviderOptions {
        id: "github-copilot".to_string(),
        name: Some("GitHub Copilot".to_string()),
        base_url: Some(base.to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "GitHub Copilot token",
                vec!["COPILOT_GITHUB_TOKEN".to_string()],
            )),
            oauth: Some(crate::auth_flows::GitHubCopilotOAuth::new()),
        },
        models: catalog_models("github-copilot"),
        api: crate::models::ProviderApiSpec::ByApi(streams),
        filter_models: None,
    })
}

pub fn cloudflare_ai_gateway_provider() -> Provider {
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "anthropic-messages".to_string(),
        crate::api::cloudflare::cloudflare_streams(anthropic_streams_from_model()),
    );
    streams.insert(
        "openai-completions".to_string(),
        crate::api::cloudflare::cloudflare_streams(openai_completions_streams_from_model()),
    );
    streams.insert(
        "openai-responses".to_string(),
        crate::api::cloudflare::cloudflare_streams(openai_responses_streams_from_model()),
    );
    create_provider(CreateProviderOptions {
        id: "cloudflare-ai-gateway".to_string(),
        name: Some("Cloudflare AI Gateway".to_string()),
        base_url: Some(
            crate::api::cloudflare::CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL.to_string(),
        ),
        headers: None,
        auth: crate::api::cloudflare::cloudflare_auth(
            crate::api::cloudflare::CloudflareAuthKind::AiGateway,
        ),
        models: catalog_models("cloudflare-ai-gateway"),
        api: crate::models::ProviderApiSpec::ByApi(streams),
        filter_models: None,
    })
}

pub fn cloudflare_workers_ai_provider() -> Provider {
    create_provider(CreateProviderOptions {
        id: "cloudflare-workers-ai".to_string(),
        name: Some("Cloudflare Workers AI".to_string()),
        base_url: Some(crate::api::cloudflare::CLOUDFLARE_WORKERS_AI_BASE_URL.to_string()),
        headers: None,
        auth: crate::api::cloudflare::cloudflare_auth(
            crate::api::cloudflare::CloudflareAuthKind::WorkersAi,
        ),
        models: catalog_models("cloudflare-workers-ai"),
        api: crate::models::ProviderApiSpec::Single(crate::api::cloudflare::cloudflare_streams(
            openai_completions_streams_from_model(),
        )),
        filter_models: None,
    })
}

pub fn openai_codex_provider() -> Provider {
    provider_with_env_auth(
        "openai-codex",
        "OpenAI Codex",
        Some("https://chatgpt.com/backend-api"),
        &[],
        crate::models::ProviderApiSpec::Single(openai_codex_streams()),
    )
}

/// Vertex auth: explicit Google Cloud API key or ADC (project/location env
/// vars are read by the adaptor itself). A stored key wins; otherwise any
/// ambient Google Cloud credential file makes the provider available.
pub fn google_vertex_provider() -> Provider {
    create_provider(CreateProviderOptions {
        id: "google-vertex".to_string(),
        name: Some("Google Vertex".to_string()),
        base_url: Some("https://{location}-aiplatform.googleapis.com".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(vertex_auth()),
            oauth: None,
        },
        models: catalog_models("google-vertex"),
        api: crate::models::ProviderApiSpec::Single(google_vertex_streams()),
        filter_models: None,
    })
}

fn vertex_auth() -> Arc<dyn crate::auth::ApiKeyAuth> {
    struct VertexAuth;
    impl crate::auth::ApiKeyAuth for VertexAuth {
        fn name(&self) -> &str {
            "Google Cloud credentials"
        }
        fn check(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthCheck> {
            if credential.map(|c| c.key.is_some()).unwrap_or(false) {
                return Some(crate::auth::AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.is_empty());
            let has_adc = env("GOOGLE_APPLICATION_CREDENTIALS").is_some()
                || ctx
                    .env("HOME")
                    .map(|h| {
                        std::path::Path::new(&format!(
                            "{h}/.config/gcloud/application_default_credentials.json"
                        ))
                        .exists()
                    })
                    .unwrap_or(false);
            if env("GOOGLE_CLOUD_API_KEY").is_some() || has_adc {
                return Some(crate::auth::AuthCheck {
                    source: Some("Google Cloud credentials".to_string()),
                    auth_type: "api_key",
                });
            }
            None
        }
        fn resolve(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthResult> {
            if let Some(cred) = credential {
                if cred.key.is_some() {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth {
                            api_key: cred.key.clone(),
                            headers: None,
                            base_url: None,
                        },
                        env: cred.env.clone(),
                        source: Some("stored credential".to_string()),
                    });
                }
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.is_empty());
            if let Some(key) = env("GOOGLE_CLOUD_API_KEY") {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth {
                        api_key: Some(key),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some("GOOGLE_CLOUD_API_KEY".to_string()),
                });
            }
            // ADC path: no api key; the adaptor resolves the token + project
            // from the environment.
            if env("GOOGLE_APPLICATION_CREDENTIALS").is_some()
                || ctx
                    .env("HOME")
                    .map(|h| {
                        std::path::Path::new(&format!(
                            "{h}/.config/gcloud/application_default_credentials.json"
                        ))
                        .exists()
                    })
                    .unwrap_or(false)
            {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("ADC".to_string()),
                });
            }
            None
        }
    }
    Arc::new(VertexAuth)
}

/// Register the built-in image API providers (idempotent) and return the
/// OpenRouter image provider catalog/implementation.
pub fn builtin_images_provider() -> crate::images::ImagesProvider {
    crate::images::register_builtin_images_api_providers();
    crate::images::openrouter_images_provider()
}

/// A `Models` collection with every built-in provider registered.
pub fn builtin_models(options: crate::models::CreateModelsOptions) -> Models {
    let models_store = options.models_store.clone();
    let models = crate::models::create_models(options);
    let local_generated_at = crate::model_catalog::get_builtin_model_data_generated_at();
    for mut provider in builtin_providers() {
        // Dynamic catalogs are persisted by the coding-agent runtime in the
        // shared ModelsStore. Keep only entries newer than the bundled
        // catalog; matching ids replace the bundled model in place and new
        // ids are appended, exactly like the upstream remote provider.
        if let Some(entry) = models_store
            .as_ref()
            .and_then(|store| store.read(&provider.id))
        {
            let is_newer = entry
                .last_modified
                .zip(local_generated_at)
                .map(|(remote, local)| remote > local)
                .unwrap_or(false);
            if is_newer {
                for dynamic in entry.models {
                    if let Some(index) = provider
                        .models
                        .iter()
                        .position(|model| model.id == dynamic.id)
                    {
                        provider.models[index] = dynamic;
                    } else {
                        provider.models.push(dynamic);
                    }
                }
            }
        }
        models.set_provider(provider);
    }
    models
}

/// Typed read of the generated built-in catalog (delegates to catalog read).
pub use crate::model_catalog::get_builtin_model;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_registered() {
        let providers = builtin_providers();
        assert_eq!(providers.len(), 39);
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        for expected in [
            "google",
            "anthropic",
            "openai",
            "deepseek",
            "xai",
            "groq",
            "openrouter",
            "openai-codex",
            "github-copilot",
            "cloudflare-ai-gateway",
            "mistral",
            "together",
            "zai",
            "xiaomi",
            "qwen-token-plan-cn",
        ] {
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
            assert!(
                p.auth.api_key.is_some() || p.auth.oauth.is_some(),
                "{} has no auth",
                p.id
            );
        }
        let google = providers.iter().find(|p| p.id == "google").unwrap();
        assert!(google.auth.api_key.is_some());
    }

    #[cfg(test)]
    #[test]
    fn anthropic_provider_streams_error_without_key() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
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
    fn google_provider_uses_real_adaptor() {
        // The google provider must route through the Google Generative AI
        // adaptor (missing key -> "No API key" error), not the openai-
        // completions fallback or "no API implementation".
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("GEMINI_API_KEY");
            let provider = google_provider();
            let model = provider.models.first().cloned().unwrap();
            assert_eq!(
                model.api, "google-generative-ai",
                "google catalog models must declare the google api"
            );
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            let acceptable = err.contains("No API key")
                || err.contains("not configured")
                || err.contains("Provider is not configured");
            assert!(acceptable, "got: {err}");
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn openai_provider_routes_through_responses_adaptor() {
        // Upstream openaiProvider uses openAIResponsesApi as its single api.
        // The no-key path must surface the responses adaptor's error, not the
        // completions adaptor's or "no API implementation".
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("OPENAI_API_KEY");
            let provider = openai_provider();
            let model = provider.models.first().cloned().unwrap();
            assert_eq!(model.provider, "openai");
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("No API key for provider: openai"),
                "got: {err}"
            );
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn azure_provider_routes_through_azure_adaptor() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("AZURE_OPENAI_API_KEY");
            let provider = azure_openai_responses_provider();
            let model = provider.models.first().cloned().unwrap();
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("No API key for provider: azure-openai-responses"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn mistral_provider_routes_through_mistral_adaptor() {
        // Upstream mistralProvider uses mistralConversationsApi as its single
        // api. The no-key path must surface the mistral-conversations adaptor's
        // error, not the openai-completions fallback or "no API implementation".
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("MISTRAL_API_KEY");
            let provider = mistral_provider();
            let model = provider.models.first().cloned().unwrap();
            assert_eq!(
                model.api, "mistral-conversations",
                "mistral catalog models must declare the mistral api"
            );
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("No API key for provider: mistral"),
                "got: {err}"
            );
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn openai_codex_provider_routes_through_codex_adaptor() {
        // openai-codex must dispatch through the codex-responses adaptor. The
        // provider has no ambient api key (OAuth is not ported), so the
        // no-key path surfaces the adaptor's error rather than "no API
        // implementation".
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let provider = openai_codex_provider();
            let model = provider.models.first().cloned().unwrap();
            assert_eq!(
                model.api, "openai-codex-responses",
                "codex catalog models must declare the codex api"
            );
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("No API key for provider: openai-codex"),
                "got: {err}"
            );
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn opencode_mixed_api_dispatches_by_model_api() {
        let provider = opencode_provider();
        let models = provider.get_models();
        // The opencode catalog carries multiple apis; the provider must
        // dispatch each model to its own stream.
        let mut apis = std::collections::BTreeSet::new();
        for m in &models {
            apis.insert(m.api.clone());
        }
        assert!(apis.len() >= 2, "expected mixed apis, got {apis:?}");
        for m in models.iter().take(5) {
            let streams = provider.streams.clone();
            let has_entry = streams.get(&m.api).is_some();
            assert!(
                has_entry,
                "model {} api {} missing provider stream",
                m.id, m.api
            );
        }
    }

    #[test]
    fn builtin_models_facade_lists_all_models() {
        let models = builtin_models(crate::models::CreateModelsOptions::default());
        let all = models.get_models(None);
        assert_eq!(all.len(), 1267);
        assert!(models.get_model("google", "gemini-2.5-flash").is_some());
        assert!(models.get_model("anthropic", "claude-sonnet-4-6").is_some());
    }

    #[allow(clippy::await_holding_lock)]
    #[test]
    fn amazon_bedrock_routes_through_bedrock_adaptor() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
            std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK");
            std::env::remove_var("AWS_PROFILE");
            let provider = amazon_bedrock_provider();
            let model = provider
                .models
                .iter()
                .find(|m| m.api == "bedrock-converse-stream")
                .cloned()
                .unwrap();
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("Could not load credentials") || err.contains("Request failed"),
                "got: {err}"
            );
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[allow(clippy::await_holding_lock)]
    #[test]
    fn google_vertex_routes_through_vertex_adaptor() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("GCLOUD_PROJECT");
            std::env::remove_var("GOOGLE_CLOUD_PROJECT");
            std::env::remove_var("GOOGLE_CLOUD_LOCATION");
            let provider = google_vertex_provider();
            let model = provider
                .models
                .iter()
                .find(|m| m.api == "google-vertex")
                .cloned()
                .unwrap();
            let ctx = crate::types::Context::default();
            let stream = provider.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            assert!(
                err.contains("Vertex AI requires a project ID"),
                "got: {err}"
            );
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn cloudflare_providers_require_account_credentials() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("CLOUDFLARE_API_KEY");
            std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
            std::env::remove_var("CLOUDFLARE_GATEWAY_ID");
            let provider = cloudflare_ai_gateway_provider();
            let model = provider.models.first().cloned().unwrap();
            // apply_auth fails without api key + account/gateway ids.
            let models =
                crate::models::create_models(crate::models::CreateModelsOptions::default());
            models.set_provider(provider);
            let options = crate::types::ProviderRequestOptions::default();
            let result = models.apply_auth(&model, &options);
            assert!(
                result.is_err(),
                "expected auth failure without Cloudflare env"
            );
        });
    }

    #[test]
    fn cloudflare_ai_gateway_dispatches_by_model_api() {
        let provider = cloudflare_ai_gateway_provider();
        let mut apis = std::collections::BTreeSet::new();
        for m in provider.models.iter() {
            apis.insert(m.api.clone());
        }
        assert!(apis.contains("anthropic-messages"), "{apis:?}");
        assert!(apis.contains("openai-completions"), "{apis:?}");
        assert!(apis.contains("openai-responses"), "{apis:?}");
        for m in provider.models.iter() {
            let has = provider.streams.get(&m.api).is_some();
            assert!(has, "model {} api {} missing stream", m.id, m.api);
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[test]
    fn github_copilot_dispatches_by_model_api_and_streams() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = crate::utils::env_lock();
            std::env::remove_var("COPILOT_GITHUB_TOKEN");
            let provider = github_copilot_provider();
            let mut apis = std::collections::BTreeSet::new();
            for m in provider.models.iter() {
                apis.insert(m.api.clone());
            }
            assert!(apis.contains("anthropic-messages"), "{apis:?}");
            assert!(apis.contains("openai-completions"), "{apis:?}");
            assert!(apis.contains("openai-responses"), "{apis:?}");
            // Route through the Models facade so auth is applied; no key
            // must surface a terminal auth error, not a network request and
            // not "no API implementation".
            let models =
                crate::models::create_models(crate::models::CreateModelsOptions::default());
            models.set_provider(provider);
            let model = models
                .get_model("github-copilot", "claude-sonnet-4.6")
                .or_else(|| {
                    // Fall back to the first anthropic-messages model if the
                    // catalog id differs.
                    models
                        .get_models(Some("github-copilot"))
                        .into_iter()
                        .find(|m| m.api == "anthropic-messages")
                })
                .expect("a copilot anthropic-messages model");
            let ctx = crate::types::Context {
                system_prompt: None,
                messages: vec![crate::types::Message::User(
                    crate::types::UserContent::string("hi", 1),
                )],
                tools: vec![],
            };
            let stream = models.stream(&model, &ctx, None);
            let msg = stream.for_each(|_| {}).await;
            let err = msg.error_message().unwrap_or("").to_string();
            let acceptable = err.contains("No API key")
                || err.contains("not configured")
                || err.contains("Provider is not configured");
            assert!(acceptable, "got: {err}");
            assert!(!err.contains("no API implementation"), "got: {err}");
        });
    }

    #[test]
    fn openrouter_keeps_completions_and_images_provider_registered() {
        let provider = openrouter_provider();
        let model = provider.models.first().cloned().unwrap();
        assert_eq!(model.api, "openai-completions");
        // Image provider: catalog + registered openrouter-images implementation.
        let images = builtin_images_provider();
        assert_eq!(images.id, "openrouter");
        assert!(images.models.len() >= 36);
        // generate_images for a registered api returns non-error without a key
        // (the error path is encoded on the output).
        let model = images.models[0].clone();
        let out = crate::images::generate_images(
            &model,
            &crate::types::ImagesContext { input: vec![] },
            &crate::images::ImagesOptions::default(),
        );
        assert!(out.error_message.is_some());
    }

    #[test]
    fn builtin_models_facade_auth_gating() {
        let _guard = crate::utils::env_lock();
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

/// ProviderStreams for the google-generative-ai API family. The default
/// base URL includes `/v1beta` (the vendored catalog model base URLs carry
/// the full version path, matching upstream's apiVersion suppression).
pub fn google_streams(base_url: String, _default_base: &str) -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let go = crate::api::google_generative_ai::GoogleOptions::from_stream_options(
                    options.cloned().unwrap_or_default(),
                );
                crate::api::google_generative_ai::stream(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &go,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::google_generative_ai::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &opts,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

pub fn google_provider_real() -> Provider {
    provider_with_env_auth(
        "google",
        "Google",
        Some(crate::api::google_generative_ai::DEFAULT_BASE_URL),
        &["GEMINI_API_KEY"],
        crate::models::ProviderApiSpec::Single(google_streams(
            crate::api::google_generative_ai::DEFAULT_BASE_URL.to_string(),
            crate::api::google_generative_ai::DEFAULT_BASE_URL,
        )),
    )
}

/// ProviderStreams for the openai-responses API family.
pub fn openai_responses_streams(base_url: String) -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let opts = crate::api::openai_responses::OpenAIResponsesOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::openai_responses::stream(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &opts,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::openai_responses::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &opts,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// Anthropic Messages streams bound to an explicit base URL (for mixed-api
/// providers like opencode that route models by api).
pub fn anthropic_streams_for(base_url: &str) -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let base_url = base_url.to_string();
    let stream = {
        let client = client.clone();
        let base_url = base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
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
    crate::models::ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

// ---------------------------------------------------------------------------
// Adaptor streams for the Session 11 provider wiring
// ---------------------------------------------------------------------------

/// OpenAI-completions streams that derive the request base URL from the
/// model's resolved base URL (used by Cloudflare, whose catalog base URLs
/// carry `{CLOUDFLARE_*}` placeholders materialized per-request).
fn openai_completions_streams_from_model() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let chat_options = crate::api::openai_completions::OpenAIChatOptions {
                    base: options.cloned().unwrap_or_default(),
                    reasoning_effort: None,
                    tool_choice: None,
                    thinking_budgets: None,
                };
                crate::api::openai_completions::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &chat_options,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let Some(options) = options else {
                    return crate::event_stream::create_error_stream(
                        &model.api,
                        &model.provider,
                        &model.id,
                        "streamSimple requires options".to_string(),
                    );
                };
                crate::api::openai_completions::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    options,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// OpenAI-responses streams deriving the base URL from the model.
fn openai_responses_streams_from_model() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let opts = crate::api::openai_responses::OpenAIResponsesOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::openai_responses::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &opts,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::openai_responses::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &opts,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// Anthropic-messages streams deriving the base URL from the model.
fn anthropic_streams_from_model() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &crate::api::anthropic_messages::AnthropicOptions::default(),
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &crate::api::anthropic_messages::AnthropicOptions::default(),
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// Bedrock Converse streams (SigV4/bearer auth resolves inside the adaptor).
fn bedrock_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options
                    .and_then(|o| o.base.api_key.as_deref())
                    .map(|s| s.to_string());
                let opts = crate::api::bedrock_converse::BedrockOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::bedrock_converse::stream(
                    model,
                    ctx,
                    client.clone(),
                    api_key.as_deref(),
                    &opts,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options
                    .and_then(|o| o.base.base.api_key.as_deref())
                    .map(|s| s.to_string());
                let Some(options) = options else {
                    return crate::event_stream::create_error_stream(
                        &model.api,
                        &model.provider,
                        &model.id,
                        "streamSimple requires options".to_string(),
                    );
                };
                crate::api::bedrock_converse::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    api_key.as_deref(),
                    options,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// Google Vertex streams (API-key / ADC auth resolves inside the adaptor).
fn google_vertex_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options
                    .and_then(|o| o.base.api_key.as_deref())
                    .map(|s| s.to_string());
                let go = crate::api::google_vertex::GoogleVertexOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::google_vertex::stream(
                    model,
                    ctx,
                    client.clone(),
                    api_key.as_deref(),
                    &go,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options
                    .and_then(|o| o.base.base.api_key.as_deref())
                    .map(|s| s.to_string());
                let Some(options) = options else {
                    return crate::event_stream::create_error_stream(
                        &model.api,
                        &model.provider,
                        &model.id,
                        "streamSimple requires options".to_string(),
                    );
                };
                crate::api::google_vertex::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    api_key.as_deref(),
                    options,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// ProviderStreams for the mistral-conversations API family. The base URL is
/// read from the model (the catalog carries `https://api.mistral.ai`), so the
/// stream closures only need the reqwest client.
pub fn mistral_conversations_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let go = crate::api::mistral_conversations::MistralOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::mistral_conversations::stream(model, ctx, client.clone(), api_key, &go)
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::mistral_conversations::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    api_key,
                    &opts,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

/// ProviderStreams for the openai-codex-responses API family. The Codex URL is
/// derived from the model base URL (`resolve_codex_url`), so the stream
/// closures only need the reqwest client. Auth comes from the ChatGPT access
/// token supplied in options (OAuth is not yet ported; the provider carries no
/// ambient api key).
pub fn openai_codex_streams() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let go = crate::api::openai_codex_responses::OpenAICodexResponsesOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::openai_codex_responses::stream(model, ctx, client.clone(), api_key, &go)
            },
        )
    };
    let simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let opts = options.cloned().unwrap_or_default();
                crate::api::openai_codex_responses::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    api_key,
                    &opts,
                )
            },
        )
    };
    crate::models::ProviderStreams {
        stream,
        stream_simple: simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}
