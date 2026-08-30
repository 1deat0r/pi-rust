//! Provider registry — port of `packages/ai/src/providers/all.ts`.
//!
//! Every built-in provider is constructed from the vendored model catalog
//! (see `crate::model_catalog`) plus its upstream auth semantics. Provider
//! registrations dispatch through the concrete Rust API adaptors, including
//! the native Radius `pi-messages` transport and the supported image path.

use std::sync::Arc;

use crate::auth::{env_api_key_auth, ProviderAuth};
use crate::model::Model;
use crate::model_catalog::get_builtin_models;
use crate::models::{create_provider, CreateProviderOptions, Models, Provider};

/// Build a provider with catalog models, env-key auth, and its concrete Rust
/// stream adaptor.
pub fn provider_with_env_auth(
    id: &str,
    name: &str,
    base_url: Option<&str>,
    env_vars: &[&str],
    api: crate::models::ProviderApiSpec,
) -> Provider {
    provider_with_env_auth_label(id, name, name, base_url, env_vars, api)
}

fn provider_with_env_auth_label(
    id: &str,
    name: &str,
    auth_name: &str,
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
                auth_name,
                env_vars.iter().map(|s| s.to_string()).collect(),
            )),
            oauth: None,
        },
        models,
        api,
        filter_models: None,
    })
}

fn provider_with_env_auth_label_without_base(
    id: &str,
    name: &str,
    auth_name: &str,
    env_vars: &[&str],
    api: crate::models::ProviderApiSpec,
) -> Provider {
    create_provider(CreateProviderOptions {
        id: id.to_string(),
        name: Some(name.to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                auth_name,
                env_vars.iter().map(|s| s.to_string()).collect(),
            )),
            oauth: None,
        },
        models: catalog_models(id),
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
        radius_provider(),
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

/// Built-in Radius provider with the upstream default gateway.
pub fn radius_provider() -> Provider {
    crate::providers::radius::radius_provider(Default::default())
}

/// Construct a Radius provider for a custom gateway or provider id.
pub fn radius_provider_with_options(
    options: crate::providers::radius::RadiusProviderOptions,
) -> Provider {
    crate::providers::radius::radius_provider(options)
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
                let model_base_url = resolve_openai_completions_base_url(model, &stream_base);
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
                    model_base_url,
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
                let model_base_url = resolve_openai_completions_base_url(model, &simple_base);
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
                    model_base_url,
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

fn resolve_openai_completions_base_url<'a>(model: &'a Model, fallback: &'a str) -> &'a str {
    if model.base_url.trim().is_empty() {
        fallback
    } else {
        model.base_url.as_str()
    }
}

macro_rules! env_provider {
    ($fn_name:ident, $id:expr, $name:expr, $base:expr, $env_vars:expr) => {
        pub fn $fn_name() -> Provider {
            provider_with_env_auth_label(
                $id,
                $name,
                $name,
                Some($base),
                &$env_vars,
                crate::models::ProviderApiSpec::Single(
                    openai_completions_streams_from_model_with_default(Some($base)),
                ),
            )
        }
    };
    ($fn_name:ident, $id:expr, $name:expr, $base:expr, $env_vars:expr, $auth_name:expr) => {
        pub fn $fn_name() -> Provider {
            provider_with_env_auth_label(
                $id,
                $name,
                $auth_name,
                Some($base),
                &$env_vars,
                crate::models::ProviderApiSpec::Single(
                    openai_completions_streams_from_model_with_default(Some($base)),
                ),
            )
        }
    };
}

fn provider_with_anthropic_catalog_auth(
    id: &str,
    name: &str,
    auth_name: &str,
    base_url: &str,
    env_vars: &[&str],
) -> Provider {
    let models = catalog_models(id);
    let resolved_base_url = models
        .first()
        .map(|model| model.base_url.clone())
        .unwrap_or_else(|| base_url.to_string());
    create_provider(CreateProviderOptions {
        id: id.to_string(),
        name: Some(name.to_string()),
        base_url: Some(resolved_base_url.clone()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                auth_name,
                env_vars.iter().map(|value| (*value).to_string()).collect(),
            )),
            oauth: None,
        },
        models,
        api: crate::models::ProviderApiSpec::Single(anthropic_streams_from_model()),
        filter_models: None,
    })
}

env_provider!(
    ant_ling_provider,
    "ant-ling",
    "Ant Ling",
    "https://api.ant-ling.com/v1",
    ["ANT_LING_API_KEY"],
    "Ant Ling API key"
);
pub fn azure_openai_responses_provider() -> Provider {
    let models = catalog_models("azure-openai-responses");
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
        base_url: None,
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
    ["BASETEN_API_KEY"],
    "Baseten API key"
);
env_provider!(
    cerebras_provider,
    "cerebras",
    "Cerebras",
    "https://api.cerebras.ai/v1",
    ["CEREBRAS_API_KEY"],
    "Cerebras API key"
);
env_provider!(
    deepseek_provider,
    "deepseek",
    "DeepSeek",
    "https://api.deepseek.com",
    ["DEEPSEEK_API_KEY"],
    "DeepSeek API key"
);
pub fn fireworks_provider() -> Provider {
    let models = catalog_models("fireworks");
    let base_url = models
        .first()
        .map(|model| model.base_url.clone())
        .unwrap_or_else(|| "https://api.fireworks.ai/inference".to_string());
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_from_model(),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams_from_model_with_default(Some(&base_url)),
    );
    create_provider(CreateProviderOptions {
        id: "fireworks".to_string(),
        name: Some("Fireworks".to_string()),
        base_url: Some(base_url),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Fireworks API key",
                vec!["FIREWORKS_API_KEY".to_string()],
            )),
            oauth: None,
        },
        models,
        api: crate::models::ProviderApiSpec::ByApi(streams),
        filter_models: None,
    })
}
pub fn google_provider() -> Provider {
    google_provider_real()
}
env_provider!(
    groq_provider,
    "groq",
    "Groq",
    "https://api.groq.com/openai/v1",
    ["GROQ_API_KEY"],
    "Groq API key"
);
env_provider!(
    huggingface_provider,
    "huggingface",
    "Hugging Face",
    "https://router.huggingface.co/v1",
    ["HF_TOKEN"],
    "Hugging Face token"
);

const KIMI_CODING_OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_CODING_OAUTH_DEFAULT_HOST: &str = "https://auth.kimi.com";
const KIMI_CODING_OAUTH_DEFAULT_INTERVAL_SECONDS: f64 = 5.0;
const KIMI_CODING_OAUTH_DEFAULT_EXPIRES_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone)]
struct KimiDeviceAuthorization {
    device_code: String,
    user_code: String,
    _verification_uri: String,
    verification_uri_complete: String,
    interval_seconds: f64,
    expires_in_seconds: u64,
}

#[derive(Debug, Clone)]
struct KimiTokenResponse {
    access: String,
    refresh: String,
    expires: u64,
}

#[derive(Clone)]
struct KimiCodingOAuth {
    endpoint_override: Option<String>,
}

impl KimiCodingOAuth {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            endpoint_override: None,
        })
    }

    fn oauth_host(&self) -> String {
        self.endpoint_override.clone().unwrap_or_else(|| {
            std::env::var("KIMI_CODE_OAUTH_HOST")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    std::env::var("KIMI_OAUTH_HOST")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                })
                .unwrap_or_else(|| KIMI_CODING_OAUTH_DEFAULT_HOST.to_string())
                .trim_end_matches('/')
                .to_string()
        })
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    async fn response_text(
        request: reqwest::RequestBuilder,
        signal: &crate::types::AbortSignal,
        operation: &str,
        cancel_message: &str,
    ) -> Result<(reqwest::StatusCode, String), String> {
        let response =
            crate::api::openai_completions::abortable(request.send(), Some(signal.clone()))
                .await
                .map_err(|_| cancel_message.to_string())?
                .map_err(|_| format!("{operation} request failed"))?;
        let status = response.status();
        let text = crate::api::openai_completions::abortable(response.text(), Some(signal.clone()))
            .await
            .map_err(|_| cancel_message.to_string())?
            .map_err(|_| format!("{operation} response read failed"))?;
        Ok((status, text))
    }

    async fn wait_for_refresh_abort(signal: &std::sync::atomic::AtomicBool) {
        while !signal.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn response_text_ref(
        request: reqwest::RequestBuilder,
        signal: &std::sync::atomic::AtomicBool,
        operation: &str,
        cancel_message: &str,
    ) -> Result<(reqwest::StatusCode, String), String> {
        if signal.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(cancel_message.to_string());
        }
        let request = request.send();
        tokio::pin!(request);
        let response = tokio::select! {
            response = &mut request => response.map_err(|_| format!("{operation} request failed"))?,
            _ = Self::wait_for_refresh_abort(signal) => return Err(cancel_message.to_string()),
        };
        let status = response.status();
        let body = response.text();
        tokio::pin!(body);
        let text = tokio::select! {
            text = &mut body => text.map_err(|_| format!("{operation} response read failed"))?,
            _ = Self::wait_for_refresh_abort(signal) => return Err(cancel_message.to_string()),
        };
        Ok((status, text))
    }

    fn redact_detail(detail: &str, secrets: &[&str]) -> String {
        let mut detail = detail.to_string();
        for secret in secrets {
            if !secret.is_empty() {
                detail = detail.replace(secret, "<redacted>");
            }
        }
        let end = detail
            .char_indices()
            .nth(512)
            .map(|(index, _)| index)
            .unwrap_or(detail.len());
        detail.truncate(end);
        detail
    }

    fn status_detail(status: reqwest::StatusCode, body: &str, secrets: &[&str]) -> String {
        let body = Self::redact_detail(body.trim(), secrets);
        if body.is_empty() {
            format!("status {}", status.as_u16())
        } else {
            format!("status {}: {body}", status.as_u16())
        }
    }

    fn trusted_http_url(value: Option<&serde_json::Value>) -> Option<String> {
        let value = value?.as_str().filter(|value| !value.is_empty())?;
        let url = url::Url::parse(value).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return None;
        }
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        Some(url.to_string())
    }

    async fn start_device_authorization(
        &self,
        client: &reqwest::Client,
        signal: &crate::types::AbortSignal,
    ) -> Result<KimiDeviceAuthorization, String> {
        let url = format!("{}/api/oauth/device_authorization", self.oauth_host());
        let (status, body) = Self::response_text(
            client
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .form(&[("client_id", KIMI_CODING_OAUTH_CLIENT_ID)]),
            signal,
            "Kimi Code device authorization",
            "Login cancelled",
        )
        .await?;
        if !status.is_success() {
            return Err(format!(
                "Kimi Code device authorization failed with {}",
                Self::status_detail(status, &body, &[])
            ));
        }
        let json =
            serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
        let device_code = json.get("device_code").and_then(|value| value.as_str());
        let user_code = json.get("user_code").and_then(|value| value.as_str());
        let verification_uri = Self::trusted_http_url(json.get("verification_uri"));
        let verification_uri_complete =
            Self::trusted_http_url(json.get("verification_uri_complete"));
        let (
            Some(device_code),
            Some(user_code),
            Some(verification_uri),
            Some(verification_uri_complete),
        ) = (
            device_code,
            user_code,
            verification_uri,
            verification_uri_complete,
        )
        else {
            return Err("Invalid Kimi Code device authorization response".to_string());
        };
        let interval_seconds = json
            .get("interval")
            .and_then(|value| value.as_f64())
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(KIMI_CODING_OAUTH_DEFAULT_INTERVAL_SECONDS);
        let expires_in_seconds = json
            .get("expires_in")
            .and_then(|value| value.as_f64())
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.floor().min(u64::MAX as f64) as u64)
            .filter(|value| *value > 0)
            .unwrap_or(KIMI_CODING_OAUTH_DEFAULT_EXPIRES_SECONDS);
        Ok(KimiDeviceAuthorization {
            device_code: device_code.to_string(),
            user_code: user_code.to_string(),
            _verification_uri: verification_uri,
            verification_uri_complete,
            interval_seconds,
            expires_in_seconds,
        })
    }

    fn parse_token_response(
        json: &serde_json::Value,
        operation: &str,
    ) -> Result<KimiTokenResponse, String> {
        let access = json
            .get("access_token")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty());
        let refresh = json
            .get("refresh_token")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty());
        let expires_in = json
            .get("expires_in")
            .and_then(|value| value.as_f64())
            .filter(|value| value.is_finite() && *value > 0.0);
        let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
            return Err(format!(
                "Kimi Code token {operation} response missing fields"
            ));
        };
        let expires_ms = expires_in * 1000.0;
        if !expires_ms.is_finite() || expires_ms > u64::MAX as f64 {
            return Err(format!(
                "Kimi Code token {operation} response missing fields"
            ));
        }
        Ok(KimiTokenResponse {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires: crate::types::now_ms().saturating_add(expires_ms as u64),
        })
    }

    async fn poll_for_token(
        &self,
        client: &reqwest::Client,
        device: &KimiDeviceAuthorization,
        signal: &crate::types::AbortSignal,
    ) -> Result<KimiTokenResponse, String> {
        let token_url = format!("{}/api/oauth/token", self.oauth_host());
        let device_code = device.device_code.clone();
        let client_for_poll = client.clone();
        let signal_for_poll = signal.clone();
        let mut options =
            crate::oauth::DeviceCodePollOptions::<KimiTokenResponse>::new(Box::new(move || {
                let client = client_for_poll.clone();
                let token_url = token_url.clone();
                let device_code = device_code.clone();
                let signal = signal_for_poll.clone();
                Box::pin(async move {
                    let (status, body) = match Self::response_text(
                        client
                            .post(token_url)
                            .header("Content-Type", "application/x-www-form-urlencoded")
                            .header("Accept", "application/json")
                            .form(&[
                                ("client_id", KIMI_CODING_OAUTH_CLIENT_ID),
                                ("device_code", device_code.as_str()),
                                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                            ]),
                        &signal,
                        "Kimi Code device token",
                        "Login cancelled",
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => {
                            return crate::oauth::DeviceCodePollResult::Failed { message: error };
                        }
                    };
                    let json = serde_json::from_str::<serde_json::Value>(&body)
                        .unwrap_or(serde_json::Value::Null);
                    if status.as_u16() >= 500 {
                        return crate::oauth::DeviceCodePollResult::Failed {
                            message: format!(
                                "Kimi Code device token request failed with {}",
                                Self::status_detail(status, &body, &[device_code.as_str()])
                            ),
                        };
                    }
                    if status.is_success() && json.get("access_token").is_some() {
                        return match Self::parse_token_response(&json, "poll") {
                            Ok(token) => crate::oauth::DeviceCodePollResult::Complete(token),
                            Err(error) => {
                                crate::oauth::DeviceCodePollResult::Failed { message: error }
                            }
                        };
                    }
                    let error = json.get("error").and_then(|value| value.as_str());
                    let description = json
                        .get("error_description")
                        .and_then(|value| value.as_str())
                        .filter(|value| !value.is_empty())
                        .map(|value| format!(": {value}"))
                        .unwrap_or_default();
                    match error {
                        Some("authorization_pending") => {
                            crate::oauth::DeviceCodePollResult::Pending
                        }
                        Some("slow_down") => crate::oauth::DeviceCodePollResult::SlowDown {
                            interval_seconds: json
                                .get("interval")
                                .and_then(|value| value.as_f64())
                                .filter(|value| value.is_finite() && *value > 0.0),
                        },
                        Some("expired_token") => crate::oauth::DeviceCodePollResult::Failed {
                            message:
                                "Kimi Code device authorization expired. Please restart login."
                                    .to_string(),
                        },
                        Some("access_denied") => crate::oauth::DeviceCodePollResult::Failed {
                            message: "Kimi Code login was denied.".to_string(),
                        },
                        Some(error) => crate::oauth::DeviceCodePollResult::Failed {
                            message: format!(
                                "Kimi Code device token request failed (status {}){}: {error}",
                                status.as_u16(),
                                description
                            ),
                        },
                        None => crate::oauth::DeviceCodePollResult::Failed {
                            message: format!(
                                "Kimi Code device token request failed with {}",
                                Self::status_detail(status, &body, &[device_code.as_str()])
                            ),
                        },
                    }
                })
            }));
        options.interval_seconds = Some(device.interval_seconds);
        options.expires_in_seconds = Some(device.expires_in_seconds);
        options.wait_before_first_poll = true;
        options.signal = Some(signal.clone());
        crate::oauth::poll_oauth_device_code_flow(&mut options).await
    }

    async fn refresh_token(
        &self,
        client: &reqwest::Client,
        refresh_token: &str,
        signal: &std::sync::atomic::AtomicBool,
    ) -> Result<KimiTokenResponse, String> {
        let token_url = format!("{}/api/oauth/token", self.oauth_host());
        let mut last_error = None;
        for attempt in 0..=3u32 {
            if attempt > 0 {
                let delay = tokio::time::sleep(std::time::Duration::from_millis(
                    1_000u64.saturating_mul(2u64.saturating_pow(attempt - 1)),
                ));
                tokio::pin!(delay);
                tokio::select! {
                    _ = &mut delay => {}
                    _ = Self::wait_for_refresh_abort(signal) => {
                        return Err("Kimi Code token refresh aborted".to_string());
                    }
                }
            }
            if signal.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("Kimi Code token refresh aborted".to_string());
            }
            let response = Self::response_text_ref(
                client
                    .post(&token_url)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .header("Accept", "application/json")
                    .form(&[
                        ("client_id", KIMI_CODING_OAUTH_CLIENT_ID),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                    ]),
                signal,
                "Kimi Code token refresh",
                "Kimi Code token refresh aborted",
            )
            .await;
            let response = match response {
                Ok(response) => response,
                Err(error) if error == "Kimi Code token refresh aborted" => return Err(error),
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            let (status, body) = response;
            let json =
                serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
            if status.is_success() {
                return Self::parse_token_response(&json, "refresh");
            }
            let error_code = json.get("error").and_then(|value| value.as_str());
            let description = json
                .get("error_description")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            if status.as_u16() == 401
                || status.as_u16() == 403
                || error_code == Some("invalid_grant")
            {
                return Err(format!(
                    "Kimi Code token refresh unauthorized (status {}){}",
                    status.as_u16(),
                    description
                ));
            }
            if (status.as_u16() == 429 || status.as_u16() >= 500) && attempt < 3 {
                last_error = Some(format!(
                    "Kimi Code token refresh failed with {}",
                    Self::status_detail(status, &body, &[refresh_token])
                ));
                continue;
            }
            return Err(format!(
                "Kimi Code token refresh failed with {}",
                Self::status_detail(status, &body, &[refresh_token])
            ));
        }
        Err(last_error.unwrap_or_else(|| "Kimi Code token refresh failed".to_string()))
    }
}

#[async_trait::async_trait]
impl crate::auth::OAuthAuth for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    async fn login(
        &self,
        interaction: &dyn crate::auth::AuthInteraction,
    ) -> Result<crate::auth::OAuthCredential, String> {
        let signal = interaction
            .signal()
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        if signal.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Login cancelled".to_string());
        }
        let client = Self::client();
        let device = self.start_device_authorization(&client, &signal).await?;
        interaction.notify(&crate::auth::AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri_complete.clone(),
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(device.expires_in_seconds),
        });
        let token = self.poll_for_token(&client, &device, &signal).await?;
        Ok(crate::auth::OAuthCredential {
            refresh: token.refresh,
            access: token.access,
            expires: token.expires,
            extra: std::collections::BTreeMap::new(),
        })
    }

    async fn refresh(
        &self,
        credential: &crate::auth::OAuthCredential,
        signal: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::auth::OAuthCredential, String> {
        let token = self
            .refresh_token(&Self::client(), &credential.refresh, signal)
            .await?;
        Ok(crate::auth::OAuthCredential {
            refresh: token.refresh,
            access: token.access,
            expires: token.expires,
            extra: credential.extra.clone(),
        })
    }

    fn to_auth(&self, credential: &crate::auth::OAuthCredential) -> Option<crate::auth::ModelAuth> {
        Some(crate::auth::ModelAuth {
            api_key: None,
            headers: Some(std::collections::BTreeMap::from([(
                "Authorization".to_string(),
                Some(format!("Bearer {}", credential.access)),
            )])),
            base_url: None,
        })
    }
}

fn kimi_coding_oauth() -> Arc<dyn crate::auth::OAuthAuth> {
    KimiCodingOAuth::new()
}

pub fn kimi_coding_provider() -> Provider {
    let mut provider = provider_with_anthropic_catalog_auth(
        "kimi-coding",
        "Kimi For Coding",
        "Kimi API key",
        "https://api.kimi.com/coding",
        &["KIMI_API_KEY"],
    );
    provider.auth.oauth = Some(kimi_coding_oauth());
    provider
}
pub fn minimax_provider() -> Provider {
    provider_with_anthropic_catalog_auth(
        "minimax",
        "MiniMax",
        "MiniMax API key",
        "https://api.minimax.io/anthropic",
        &["MINIMAX_API_KEY"],
    )
}
pub fn minimax_cn_provider() -> Provider {
    provider_with_anthropic_catalog_auth(
        "minimax-cn",
        "MiniMax CN",
        "MiniMax CN API key",
        "https://api.minimaxi.com/anthropic",
        &["MINIMAX_CN_API_KEY"],
    )
}
pub fn mistral_provider() -> Provider {
    provider_with_env_auth_label(
        "mistral",
        "Mistral",
        "Mistral API key",
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
    ["MOONSHOT_API_KEY"],
    "Moonshot AI API key"
);
env_provider!(
    moonshotai_cn_provider,
    "moonshotai-cn",
    "Moonshot AI CN",
    "https://api.moonshot.cn/v1",
    ["MOONSHOT_API_KEY"],
    "Moonshot AI API key"
);
env_provider!(
    nvidia_provider,
    "nvidia",
    "NVIDIA",
    "https://integrate.api.nvidia.com/v1",
    ["NVIDIA_API_KEY"],
    "NVIDIA API key"
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
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_from_model(),
    );
    streams.insert(
        "google-generative-ai".to_string(),
        google_streams_from_model(),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams_from_model_with_default(Some("https://opencode.ai/zen/v1")),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams_from_model(),
    );
    provider_with_env_auth_label_without_base(
        "opencode",
        "OpenCode Zen",
        "OpenCode API key",
        &["OPENCODE_API_KEY"],
        crate::models::ProviderApiSpec::ByApi(streams),
    )
}
pub fn opencode_go_provider() -> Provider {
    let mut streams = std::collections::BTreeMap::new();
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_from_model(),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams_from_model_with_default(Some("https://opencode.ai/zen/go/v1")),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams_from_model(),
    );
    provider_with_env_auth_label_without_base(
        "opencode-go",
        "OpenCode Go",
        "OpenCode API key",
        &["OPENCODE_API_KEY"],
        crate::models::ProviderApiSpec::ByApi(streams),
    )
}
pub fn openrouter_provider() -> Provider {
    let mut provider = provider_with_env_auth_label(
        "openrouter",
        "OpenRouter",
        "OpenRouter API key",
        Some("https://openrouter.ai/api/v1"),
        &["OPENROUTER_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_completions_streams_from_model_with_default(
            Some("https://openrouter.ai/api/v1"),
        )),
    );
    provider.auth.oauth = Some(crate::auth_flows::OpenRouterOAuth::new());
    provider
}
pub fn qwen_token_plan_provider() -> Provider {
    provider_with_env_auth_label(
        "qwen-token-plan",
        "Qwen Token Plan",
        "Qwen Token Plan API key",
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        &["QWEN_TOKEN_PLAN_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_completions_streams_from_model_with_default(
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        )),
    )
}

pub fn qwen_token_plan_cn_provider() -> Provider {
    provider_with_env_auth_label(
        "qwen-token-plan-cn",
        "Qwen Token Plan CN",
        "Qwen Token Plan CN API key",
        Some("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
        &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_completions_streams_from_model_with_default(
            Some("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
        )),
    )
}

pub fn qwen_token_plan_individual_provider() -> Provider {
    provider_with_env_auth_label(
        "qwen-token-plan-individual",
        "Qwen Token Plan Individual",
        "Qwen Token Plan Individual API key",
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        &["QWEN_TOKEN_PLAN_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_completions_streams_from_model_with_default(
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        )),
    )
}
env_provider!(
    together_provider,
    "together",
    "Together",
    "https://api.together.ai/v1",
    ["TOGETHER_API_KEY"],
    "Together API key"
);
pub fn vercel_ai_gateway_provider() -> Provider {
    provider_with_env_auth_label(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        "Vercel AI Gateway API key",
        Some("https://ai-gateway.vercel.sh"),
        &["AI_GATEWAY_API_KEY"],
        crate::models::ProviderApiSpec::Single(anthropic_streams_for(
            "https://ai-gateway.vercel.sh",
        )),
    )
}
pub fn xai_provider() -> Provider {
    let mut provider = provider_with_env_auth_label(
        "xai",
        "xAI",
        "xAI API key",
        Some("https://api.x.ai/v1"),
        &["XAI_API_KEY"],
        crate::models::ProviderApiSpec::Single(openai_responses_streams(
            "https://api.x.ai/v1".to_string(),
        )),
    );
    provider.auth.oauth = Some(crate::auth_flows::XaiOAuth::new());
    provider
}
env_provider!(
    xiaomi_provider,
    "xiaomi",
    "Xiaomi",
    "https://api.xiaomimimo.com/v1",
    ["XIAOMI_API_KEY"],
    "Xiaomi API key"
);
env_provider!(
    xiaomi_token_plan_ams_provider,
    "xiaomi-token-plan-ams",
    "Xiaomi Token Plan AMS",
    "https://token-plan-ams.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
    "Xiaomi Token Plan AMS API key"
);
env_provider!(
    xiaomi_token_plan_cn_provider,
    "xiaomi-token-plan-cn",
    "Xiaomi Token Plan CN",
    "https://token-plan-cn.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
    "Xiaomi Token Plan CN API key"
);
env_provider!(
    xiaomi_token_plan_sgp_provider,
    "xiaomi-token-plan-sgp",
    "Xiaomi Token Plan SGP",
    "https://token-plan-sgp.xiaomimimo.com/v1",
    ["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
    "Xiaomi Token Plan SGP API key"
);
env_provider!(
    zai_provider,
    "zai",
    "Z.AI",
    "https://api.z.ai/api/paas/v4",
    ["ZAI_API_KEY"],
    "Z.AI API key"
);
env_provider!(
    zai_coding_cn_provider,
    "zai-coding-cn",
    "Z.AI Coding CN",
    "https://open.bigmodel.cn/api/coding/paas/v4",
    ["ZAI_CODING_CN_API_KEY"],
    "Z.AI Coding CN API key"
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
            api_key: Some(anthropic_api_key_auth()),
            oauth: Some(crate::auth_flows::AnthropicOAuth::new()),
        },
        models,
        api: crate::models::ProviderApiSpec::Single(anthropic_streams_from_model()),
        filter_models: None,
    })
}

/// Anthropic's ambient auth has a different header contract from ordinary
/// API-key providers: AUTH_TOKEN must become `Authorization: Bearer`, while
/// OAUTH_TOKEN and API_KEY remain API-adapter keys (the adapter recognizes the
/// OAuth token prefix and selects Bearer itself). Keep this distinction in the
/// shared Models facade so `/login`, availability checks, and real requests
/// all see the same ordered sources.
fn anthropic_api_key_auth() -> Arc<dyn crate::auth::ApiKeyAuth> {
    struct AnthropicApiKeyAuth;

    impl crate::auth::ApiKeyAuth for AnthropicApiKeyAuth {
        fn name(&self) -> &str {
            "Anthropic API key"
        }

        fn check(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthCheck> {
            if credential
                .and_then(|credential| credential.key.as_deref())
                .is_some_and(|key| !key.trim().is_empty())
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            [
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
            ]
            .into_iter()
            .find_map(|env_var| {
                ctx.env(env_var)
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| crate::auth::AuthCheck {
                        source: Some(env_var.to_string()),
                        auth_type: "api_key",
                    })
            })
        }

        fn resolve(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthResult> {
            if let Some(credential) = credential {
                if credential
                    .key
                    .as_deref()
                    .is_some_and(|key| !key.trim().is_empty())
                {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth {
                            api_key: credential.key.clone(),
                            headers: None,
                            base_url: None,
                        },
                        env: credential.env.clone(),
                        source: Some("stored credential".to_string()),
                    });
                }
            }

            if let Some(token) = ctx
                .env("ANTHROPIC_AUTH_TOKEN")
                .filter(|value| !value.trim().is_empty())
            {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth {
                        api_key: None,
                        headers: Some(std::collections::BTreeMap::from([(
                            "authorization".to_string(),
                            Some(format!("Bearer {token}")),
                        )])),
                        base_url: None,
                    },
                    env: None,
                    source: Some("ANTHROPIC_AUTH_TOKEN".to_string()),
                });
            }

            for env_var in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
                if let Some(value) = ctx.env(env_var).filter(|value| !value.trim().is_empty()) {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth {
                            api_key: Some(value),
                            headers: None,
                            base_url: None,
                        },
                        env: None,
                        source: Some(env_var.to_string()),
                    });
                }
            }
            None
        }
    }

    Arc::new(AnthropicApiKeyAuth)
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
        fn login(
            &self,
            interaction: &dyn crate::auth::AuthInteraction,
        ) -> Result<crate::auth::ApiKeyCredential, String> {
            if interaction
                .signal()
                .as_ref()
                .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Err("Login cancelled".to_string());
            }
            let method = interaction.prompt(&crate::auth::AuthPrompt::Select {
                message: "Select Amazon Bedrock authentication method:".to_string(),
                options: vec![
                    crate::auth::AuthSelectOption {
                        id: "bearer-token".to_string(),
                        label: "Bearer token".to_string(),
                        description: None,
                    },
                    crate::auth::AuthSelectOption {
                        id: "aws-profile".to_string(),
                        label: "AWS profile".to_string(),
                        description: None,
                    },
                    crate::auth::AuthSelectOption {
                        id: "credential-chain".to_string(),
                        label: "Existing AWS credential chain".to_string(),
                        description: None,
                    },
                ],
            })?;
            if interaction
                .signal()
                .as_ref()
                .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Err("Login cancelled".to_string());
            }
            if method == "bearer-token" {
                return Ok(crate::auth::ApiKeyCredential {
                    key: Some(interaction.prompt(&crate::auth::AuthPrompt::Secret {
                        message: "Enter Amazon Bedrock bearer token".to_string(),
                        placeholder: None,
                    })?),
                    env: None,
                });
            }
            interaction.notify(&crate::auth::AuthEvent::Info {
                message: "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials."
                    .to_string(),
                links: vec![crate::auth::AuthInfoLink {
                    url: "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html"
                        .to_string(),
                    label: Some("AWS credential provider chain".to_string()),
                }],
            });
            match method.as_str() {
                "aws-profile" => Ok(crate::auth::ApiKeyCredential {
                    key: None,
                    env: Some(std::collections::BTreeMap::from([(
                        "AWS_PROFILE".to_string(),
                        interaction.prompt(&crate::auth::AuthPrompt::Text {
                            message: "Enter AWS profile name".to_string(),
                            placeholder: None,
                        })?,
                    )])),
                }),
                "credential-chain" => {
                    interaction.prompt(&crate::auth::AuthPrompt::Text {
                        message: "Configure AWS credentials, then press Enter to continue"
                            .to_string(),
                        placeholder: None,
                    })?;
                    Ok(crate::auth::ApiKeyCredential {
                        key: None,
                        env: None,
                    })
                }
                _ => Err(format!("Unknown Amazon Bedrock auth method: {method}")),
            }
        }
        fn check(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthCheck> {
            if credential
                .and_then(|credential| credential.key.as_deref())
                .is_some_and(|key| !key.trim().is_empty())
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            if credential.and_then(|c| c.env.as_ref()).is_some_and(|e| {
                e.get("AWS_PROFILE")
                    .is_some_and(|profile| !profile.trim().is_empty())
            }) {
                return Some(crate::auth::AuthCheck {
                    source: Some("AWS_PROFILE".to_string()),
                    auth_type: "api_key",
                });
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.trim().is_empty());
            if env("AWS_BEARER_TOKEN_BEDROCK").is_some()
                || env("AWS_PROFILE").is_some()
                || (env("AWS_ACCESS_KEY_ID").is_some() && env("AWS_SECRET_ACCESS_KEY").is_some())
                || env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                || env("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
                || env("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
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
                if cred
                    .key
                    .as_deref()
                    .is_some_and(|key| !key.trim().is_empty())
                {
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
                if cred.env.as_ref().is_some_and(|e| {
                    e.get("AWS_PROFILE")
                        .is_some_and(|profile| !profile.trim().is_empty())
                }) {
                    return Some(crate::auth::AuthResult {
                        auth: crate::auth::ModelAuth::default(),
                        env: cred.env.clone(),
                        source: Some("stored credential".to_string()),
                    });
                }
            }
            let env = |name: &str| ctx.env(name).filter(|v| !v.trim().is_empty());
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
            if env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                || env("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
            {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("ECS task role".to_string()),
                });
            }
            if env("AWS_WEB_IDENTITY_TOKEN_FILE").is_some() {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: None,
                    source: Some("web identity token".to_string()),
                });
            }
            None
        }
    }
    Arc::new(BedrockAuth)
}

pub fn github_copilot_provider() -> Provider {
    github_copilot_provider_with_oauth(crate::oauth::GitHubCopilotOAuth::new())
}

/// Build the Copilot provider with an injectable OAuth implementation. The
/// production constructor above supplies the real network implementation;
/// the seam keeps auth-storage and model-filter parity fixtures deterministic.
pub fn github_copilot_provider_with_oauth(oauth: Arc<dyn crate::auth::OAuthAuth>) -> Provider {
    let mut streams = std::collections::BTreeMap::new();
    let base = "https://api.individual.githubcopilot.com";
    streams.insert(
        "anthropic-messages".to_string(),
        anthropic_streams_from_model(),
    );
    streams.insert(
        "openai-completions".to_string(),
        openai_completions_streams_from_model_with_default(Some(base)),
    );
    streams.insert(
        "openai-responses".to_string(),
        openai_responses_streams_from_model(),
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
            oauth: Some(oauth),
        },
        models: catalog_models("github-copilot"),
        api: crate::models::ProviderApiSpec::ByApi(streams),
        filter_models: Some(Arc::new(|models, credential| {
            let Some(crate::auth::Credential::OAuth(credential)) = credential else {
                return models.to_vec();
            };
            let Some(ids) = credential
                .extra
                .get("availableModelIds")
                .and_then(serde_json::Value::as_array)
            else {
                return models.to_vec();
            };
            if !ids.iter().all(serde_json::Value::is_string) {
                return models.to_vec();
            }
            models
                .iter()
                .filter(|model| {
                    ids.iter()
                        .filter_map(serde_json::Value::as_str)
                        .any(|id| id == model.id)
                })
                .cloned()
                .collect()
        })),
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
    openai_codex_provider_with_oauth(crate::oauth::OpenAICodexOAuth::new())
}

/// Build OpenAI Codex with an injectable OAuth implementation. Production
/// uses the ChatGPT browser/device flow; the seam keeps provider/auth tests on
/// loopback fixtures and never requires a real account.
pub fn openai_codex_provider_with_oauth(oauth: Arc<dyn crate::auth::OAuthAuth>) -> Provider {
    let mut provider = provider_with_env_auth(
        "openai-codex",
        "OpenAI Codex",
        Some("https://chatgpt.com/backend-api"),
        &[],
        crate::models::ProviderApiSpec::Single(openai_codex_streams()),
    );
    // Codex accepts ChatGPT OAuth credentials only. Keeping the generic
    // environment-key auth attached would make `/login openai-codex` offer a
    // misleading API-key branch that the upstream provider does not expose.
    provider.auth.api_key = None;
    provider.auth.oauth = Some(oauth);
    provider
}

/// Vertex auth: explicit Google Cloud API key or file-based ADC. ADC is only
/// available when its credentials file, project, and location are present;
/// stored credential environment overrides ambient values.
pub fn google_vertex_provider() -> Provider {
    create_provider(CreateProviderOptions {
        id: "google-vertex".to_string(),
        name: Some("Google Vertex AI".to_string()),
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

fn vertex_env_value(
    ctx: &crate::auth::AuthContext,
    credential_env: Option<&crate::types::ProviderEnv>,
    name: &str,
) -> Option<String> {
    credential_env
        .and_then(|env| env.get(name))
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| ctx.env(name).filter(|value| !value.trim().is_empty()))
}

fn vertex_adc_path(
    ctx: &crate::auth::AuthContext,
    credential_env: Option<&crate::types::ProviderEnv>,
) -> String {
    let explicit = vertex_env_value(ctx, credential_env, "GOOGLE_APPLICATION_CREDENTIALS");
    let home = vertex_env_value(ctx, credential_env, "HOME");
    crate::api::google_vertex::resolve_adc_path(explicit.as_deref(), home.as_deref())
}

fn vertex_has_adc(
    ctx: &crate::auth::AuthContext,
    credential_env: Option<&crate::types::ProviderEnv>,
) -> bool {
    ctx.file_exists(&vertex_adc_path(ctx, credential_env))
}

fn vertex_is_configured_adc(
    ctx: &crate::auth::AuthContext,
    credential_env: Option<&crate::types::ProviderEnv>,
) -> bool {
    vertex_has_adc(ctx, credential_env)
        && vertex_env_value(ctx, credential_env, "GOOGLE_CLOUD_PROJECT")
            .or_else(|| vertex_env_value(ctx, credential_env, "GCLOUD_PROJECT"))
            .is_some()
        && vertex_env_value(ctx, credential_env, "GOOGLE_CLOUD_LOCATION").is_some()
}

fn vertex_auth() -> Arc<dyn crate::auth::ApiKeyAuth> {
    struct VertexAuth;
    impl crate::auth::ApiKeyAuth for VertexAuth {
        fn name(&self) -> &str {
            "Google Cloud credentials"
        }
        fn login(
            &self,
            interaction: &dyn crate::auth::AuthInteraction,
        ) -> Result<crate::auth::ApiKeyCredential, String> {
            if interaction
                .signal()
                .as_ref()
                .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Err("Login cancelled".to_string());
            }
            let method = interaction.prompt(&crate::auth::AuthPrompt::Select {
                message: "Select Google Vertex AI authentication method:".to_string(),
                options: vec![
                    crate::auth::AuthSelectOption {
                        id: "api-key".to_string(),
                        label: "Google Cloud API key".to_string(),
                        description: None,
                    },
                    crate::auth::AuthSelectOption {
                        id: "adc".to_string(),
                        label: "Application Default Credentials".to_string(),
                        description: None,
                    },
                    crate::auth::AuthSelectOption {
                        id: "service-account".to_string(),
                        label: "Service account credentials file".to_string(),
                        description: None,
                    },
                ],
            })?;
            if interaction
                .signal()
                .as_ref()
                .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Err("Login cancelled".to_string());
            }
            if method == "api-key" {
                return Ok(crate::auth::ApiKeyCredential {
                    key: Some(interaction.prompt(&crate::auth::AuthPrompt::Secret {
                        message: "Enter Google Cloud API key".to_string(),
                        placeholder: None,
                    })?),
                    env: None,
                });
            }
            if method != "adc" && method != "service-account" {
                return Err(format!("Unknown Google Vertex AI auth method: {method}"));
            }
            interaction.notify(&crate::auth::AuthEvent::Info {
                message: if method == "adc" {
                    "Run `gcloud auth application-default login`, then provide the project and location."
                } else {
                    "Provide a service account credentials file, project, and location."
                }
                .to_string(),
                links: vec![crate::auth::AuthInfoLink {
                    url: "https://cloud.google.com/docs/authentication/provide-credentials-adc"
                        .to_string(),
                    label: Some("Application Default Credentials".to_string()),
                }],
            });
            let project = interaction.prompt(&crate::auth::AuthPrompt::Text {
                message: "Enter Google Cloud project ID".to_string(),
                placeholder: None,
            })?;
            let location = interaction.prompt(&crate::auth::AuthPrompt::Text {
                message: "Enter Google Cloud location".to_string(),
                placeholder: None,
            })?;
            let credentials_path = if method == "service-account" {
                Some(interaction.prompt(&crate::auth::AuthPrompt::Text {
                    message: "Enter service account credentials file path".to_string(),
                    placeholder: None,
                })?)
            } else {
                None
            };
            let mut env = std::collections::BTreeMap::from([
                ("GOOGLE_CLOUD_PROJECT".to_string(), project),
                ("GOOGLE_CLOUD_LOCATION".to_string(), location),
            ]);
            if let Some(path) = credentials_path {
                env.insert("GOOGLE_APPLICATION_CREDENTIALS".to_string(), path);
            }
            Ok(crate::auth::ApiKeyCredential {
                key: None,
                env: Some(env),
            })
        }
        fn check(
            &self,
            ctx: &crate::auth::AuthContext,
            credential: Option<&crate::auth::ApiKeyCredential>,
        ) -> Option<crate::auth::AuthCheck> {
            if credential
                .and_then(|cred| cred.key.as_deref())
                .is_some_and(|key| !key.trim().is_empty())
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: "api_key",
                });
            }
            if ctx
                .env("GOOGLE_CLOUD_API_KEY")
                .filter(|key| !key.trim().is_empty())
                .is_some()
            {
                return Some(crate::auth::AuthCheck {
                    source: Some("GOOGLE_CLOUD_API_KEY".to_string()),
                    auth_type: "api_key",
                });
            }
            let credential_env = credential.and_then(|cred| cred.env.as_ref());
            if vertex_is_configured_adc(ctx, credential_env) {
                return Some(crate::auth::AuthCheck {
                    source: Some(if credential.is_some() {
                        "stored credential".to_string()
                    } else {
                        "gcloud application default credentials".to_string()
                    }),
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
                if cred
                    .key
                    .as_deref()
                    .is_some_and(|key| !key.trim().is_empty())
                {
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
            if let Some(key) = ctx
                .env("GOOGLE_CLOUD_API_KEY")
                .filter(|key| !key.trim().is_empty())
            {
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
            let credential_env = credential.and_then(|cred| cred.env.as_ref());
            if vertex_is_configured_adc(ctx, credential_env) {
                return Some(crate::auth::AuthResult {
                    auth: crate::auth::ModelAuth::default(),
                    env: credential.and_then(|cred| cred.env.clone()),
                    source: Some(if credential.is_some() {
                        "stored credential".to_string()
                    } else {
                        "gcloud application default credentials".to_string()
                    }),
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Keep the large catalog-dispatch fixture adjacent to its registration tests;
// production provider constructors follow in this module by design.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_registered() {
        let providers = builtin_providers();
        assert_eq!(providers.len(), 40);
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
            "radius",
        ] {
            assert!(ids.contains(&expected), "missing provider {expected}");
        }
    }

    #[test]
    fn providers_have_catalog_models() {
        let providers = builtin_providers();
        for p in &providers {
            if p.id != "radius" {
                assert!(!p.models.is_empty(), "{} has no models", p.id);
            }
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

    #[test]
    fn prov_a_provider_names_and_api_key_labels_match_upstream() {
        type ProviderMetadataCase = (
            fn() -> Provider,
            &'static str,
            &'static str,
            Option<&'static str>,
            &'static str,
        );
        let cases: &[ProviderMetadataCase] = &[
            (
                amazon_bedrock_provider,
                "amazon-bedrock",
                "Amazon Bedrock",
                None,
                "AWS credentials or bearer token",
            ),
            (
                ant_ling_provider,
                "ant-ling",
                "Ant Ling",
                Some("https://api.ant-ling.com/v1"),
                "Ant Ling API key",
            ),
            (
                anthropic_provider,
                "anthropic",
                "Anthropic",
                Some("https://api.anthropic.com"),
                "Anthropic API key",
            ),
            (
                azure_openai_responses_provider,
                "azure-openai-responses",
                "Azure OpenAI",
                None,
                "Azure OpenAI API key",
            ),
            (
                baseten_provider,
                "baseten",
                "Baseten",
                Some("https://inference.baseten.co/v1"),
                "Baseten API key",
            ),
            (
                cerebras_provider,
                "cerebras",
                "Cerebras",
                Some("https://api.cerebras.ai/v1"),
                "Cerebras API key",
            ),
            (
                cloudflare_ai_gateway_provider,
                "cloudflare-ai-gateway",
                "Cloudflare AI Gateway",
                Some(crate::api::cloudflare::CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL),
                "Cloudflare API key",
            ),
            (
                cloudflare_workers_ai_provider,
                "cloudflare-workers-ai",
                "Cloudflare Workers AI",
                Some(crate::api::cloudflare::CLOUDFLARE_WORKERS_AI_BASE_URL),
                "Cloudflare API key",
            ),
            (
                deepseek_provider,
                "deepseek",
                "DeepSeek",
                Some("https://api.deepseek.com"),
                "DeepSeek API key",
            ),
            (
                fireworks_provider,
                "fireworks",
                "Fireworks",
                Some("https://api.fireworks.ai/inference"),
                "Fireworks API key",
            ),
            (
                github_copilot_provider,
                "github-copilot",
                "GitHub Copilot",
                Some("https://api.individual.githubcopilot.com"),
                "GitHub Copilot token",
            ),
            (
                google_provider,
                "google",
                "Google",
                Some(crate::api::google_generative_ai::DEFAULT_BASE_URL),
                "Gemini API key",
            ),
            (
                google_vertex_provider,
                "google-vertex",
                "Google Vertex AI",
                Some("https://{location}-aiplatform.googleapis.com"),
                "Google Cloud credentials",
            ),
            (
                groq_provider,
                "groq",
                "Groq",
                Some("https://api.groq.com/openai/v1"),
                "Groq API key",
            ),
            (
                huggingface_provider,
                "huggingface",
                "Hugging Face",
                Some("https://router.huggingface.co/v1"),
                "Hugging Face token",
            ),
            (
                kimi_coding_provider,
                "kimi-coding",
                "Kimi For Coding",
                Some("https://api.kimi.com/coding"),
                "Kimi API key",
            ),
            (
                minimax_provider,
                "minimax",
                "MiniMax",
                Some("https://api.minimax.io/anthropic"),
                "MiniMax API key",
            ),
            (
                minimax_cn_provider,
                "minimax-cn",
                "MiniMax CN",
                Some("https://api.minimaxi.com/anthropic"),
                "MiniMax CN API key",
            ),
            (
                mistral_provider,
                "mistral",
                "Mistral",
                Some("https://api.mistral.ai"),
                "Mistral API key",
            ),
            (
                moonshotai_provider,
                "moonshotai",
                "Moonshot AI",
                Some("https://api.moonshot.ai/v1"),
                "Moonshot AI API key",
            ),
            (
                moonshotai_cn_provider,
                "moonshotai-cn",
                "Moonshot AI CN",
                Some("https://api.moonshot.cn/v1"),
                "Moonshot AI API key",
            ),
        ];
        for (constructor, id, name, base_url, auth_name) in cases {
            let provider = constructor();
            assert_eq!(provider.id, *id);
            assert_eq!(provider.name, *name);
            assert_eq!(provider.base_url.as_deref(), *base_url);
            assert_eq!(
                provider.auth.api_key.as_ref().map(|auth| auth.name()),
                Some(*auth_name)
            );
        }
        let kimi = kimi_coding_provider();
        let oauth = kimi.auth.oauth.as_ref().expect("Kimi subscription OAuth");
        assert_eq!(oauth.name(), "Kimi Code (subscription)");
        assert!(oauth.is_subscription());
        assert_eq!(oauth.login_label(), Some("Sign in with Kimi Code"));
    }

    #[test]
    fn qwen_token_plan_international_uses_official_endpoint_and_key() {
        let provider = qwen_token_plan_provider();
        assert_eq!(provider.id, "qwen-token-plan");
        assert_eq!(provider.name, "Qwen Token Plan");
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1")
        );
        assert!(!provider.models.is_empty());
        assert!(provider
            .models
            .iter()
            .any(|model| model.id == "qwen3.7-max"));
        assert!(provider
            .models
            .iter()
            .any(|model| model.id == "deepseek-v4-pro-0813"));
        assert!(provider.models.iter().all(|model| model.base_url
            == "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"));

        let auth = provider.auth.api_key.expect("international API-key auth");
        assert_eq!(auth.name(), "Qwen Token Plan API key");
        let env = std::collections::BTreeMap::from([(
            "QWEN_TOKEN_PLAN_API_KEY".to_string(),
            "sk-sp-test-only".to_string(),
        )]);
        let context = crate::auth::AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|_| false),
        };
        let resolved = auth
            .resolve(&context, None)
            .expect("international key resolves");
        assert_eq!(resolved.source.as_deref(), Some("QWEN_TOKEN_PLAN_API_KEY"));
        assert_eq!(resolved.auth.api_key.as_deref(), Some("sk-sp-test-only"));
    }

    #[test]
    fn qwen_token_plan_regions_match_upstream_auth_sources() {
        let cases = [
            (
                qwen_token_plan_provider as fn() -> Provider,
                "qwen-token-plan",
                "Qwen Token Plan",
                "Qwen Token Plan API key",
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
                "QWEN_TOKEN_PLAN_API_KEY",
            ),
            (
                qwen_token_plan_cn_provider as fn() -> Provider,
                "qwen-token-plan-cn",
                "Qwen Token Plan CN",
                "Qwen Token Plan CN API key",
                "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                "QWEN_TOKEN_PLAN_CN_API_KEY",
            ),
            (
                qwen_token_plan_individual_provider as fn() -> Provider,
                "qwen-token-plan-individual",
                "Qwen Token Plan Individual",
                "Qwen Token Plan Individual API key",
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
                "QWEN_TOKEN_PLAN_API_KEY",
            ),
        ];

        for (constructor, id, name, auth_name, base_url, env_name) in cases {
            let provider = constructor();
            assert_eq!(provider.id, id);
            assert_eq!(provider.name, name);
            assert_eq!(provider.base_url.as_deref(), Some(base_url));
            assert!(!provider.models.is_empty(), "{id} has no catalog models");
            assert!(provider
                .models
                .iter()
                .all(|model| model.base_url == base_url));

            let auth = provider.auth.api_key.expect("Qwen API-key auth");
            assert_eq!(auth.name(), auth_name);
            let env = std::collections::BTreeMap::from([(
                env_name.to_string(),
                "qwen-test-key".to_string(),
            )]);
            let context = crate::auth::AuthContext {
                env: Arc::new(move |name| env.get(name).cloned()),
                file_exists: Arc::new(|_| false),
            };
            let resolved = auth.resolve(&context, None).expect("env key resolves");
            assert_eq!(resolved.source.as_deref(), Some(env_name));
            assert_eq!(resolved.auth.api_key.as_deref(), Some("qwen-test-key"));
            let checked = auth.check(&context, None).expect("env key checks");
            assert_eq!(checked.source.as_deref(), Some(env_name));

            let empty_context = crate::auth::AuthContext {
                env: Arc::new(|_| Some("   ".to_string())),
                file_exists: Arc::new(|_| false),
            };
            assert!(auth.resolve(&empty_context, None).is_none());

            let stored = crate::auth::ApiKeyCredential {
                key: Some("stored-qwen-key".to_string()),
                env: None,
            };
            let resolved = auth
                .resolve(&context, Some(&stored))
                .expect("stored key resolves");
            assert_eq!(resolved.source.as_deref(), Some("stored credential"));
            assert_eq!(resolved.auth.api_key.as_deref(), Some("stored-qwen-key"));
        }
    }

    #[test]
    fn qwen_token_plan_catalog_ids_match_pinned_upstream() {
        let shared = [
            "MiniMax-M2.5",
            "deepseek-v3.2",
            "deepseek-v4-flash",
            "deepseek-v4-flash-0731",
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "glm-5",
            "glm-5.1",
            "glm-5.2",
            "kimi-k2.5",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "qwen3.6-flash",
            "qwen3.6-plus",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
        ];
        let individual = [
            "deepseek-v4-flash-0731",
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "glm-5.2",
            "qwen3.6-flash",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
        ];

        for (provider_id, expected) in [
            ("qwen-token-plan", shared.as_slice()),
            ("qwen-token-plan-cn", shared.as_slice()),
            ("qwen-token-plan-individual", individual.as_slice()),
        ] {
            let mut actual = catalog_models(provider_id)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>();
            actual.sort();
            let mut expected = expected
                .iter()
                .map(|id| (*id).to_string())
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(actual, expected, "catalog ids for {provider_id}");
            assert!(catalog_models(provider_id)
                .iter()
                .all(|model| model.api == "openai-completions"));
        }
    }

    #[test]
    fn every_catalog_model_has_constructor_dispatch() {
        for provider in builtin_providers() {
            if provider.single_streams.is_some() {
                continue;
            }
            for model in &provider.models {
                assert!(
                    provider.streams.contains_key(&model.api),
                    "provider {} has no constructor dispatch for model {} api {}",
                    provider.id,
                    model.id,
                    model.api
                );
            }
        }
    }

    #[test]
    fn anthropic_ambient_auth_preserves_order_and_header_contract() {
        let auth = anthropic_provider()
            .auth
            .api_key
            .expect("Anthropic ambient auth");
        for (env_name, expected_source) in [
            ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN"),
            ("ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_OAUTH_TOKEN"),
            ("ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"),
        ] {
            let env =
                std::collections::BTreeMap::from([(env_name.to_string(), "value".to_string())]);
            let ctx = crate::auth::AuthContext {
                env: Arc::new(move |name| env.get(name).cloned()),
                file_exists: Arc::new(|_| false),
            };
            let resolved = auth.resolve(&ctx, None).expect("ambient auth resolves");
            assert_eq!(resolved.source.as_deref(), Some(expected_source));
            if env_name == "ANTHROPIC_AUTH_TOKEN" {
                assert_eq!(
                    resolved
                        .auth
                        .headers
                        .as_ref()
                        .and_then(|headers| headers.get("authorization"))
                        .and_then(|value| value.as_deref()),
                    Some("Bearer value")
                );
                assert!(resolved.auth.api_key.is_none());
            } else {
                assert_eq!(resolved.auth.api_key.as_deref(), Some("value"));
                assert!(resolved.auth.headers.is_none());
            }
        }

        let env = std::collections::BTreeMap::from([
            ("ANTHROPIC_AUTH_TOKEN".to_string(), " ".to_string()),
            ("ANTHROPIC_OAUTH_TOKEN".to_string(), "oauth".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "api".to_string()),
        ]);
        let ctx = crate::auth::AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|_| false),
        };
        let resolved = auth.resolve(&ctx, None).expect("fallback auth resolves");
        assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_OAUTH_TOKEN"));
        assert_eq!(resolved.auth.api_key.as_deref(), Some("oauth"));
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
            let stream = {
                let _guard = crate::utils::env_lock();
                std::env::remove_var("GEMINI_API_KEY");
                let provider = google_provider();
                let model = provider.models.first().cloned().unwrap();
                assert_eq!(
                    model.api, "google-generative-ai",
                    "google catalog models must declare the google api"
                );
                let ctx = crate::types::Context::default();
                provider.stream(&model, &ctx, None)
            };
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
            let stream = {
                let _guard = crate::utils::env_lock();
                std::env::remove_var("OPENAI_API_KEY");
                let provider = openai_provider();
                let model = provider.models.first().cloned().unwrap();
                assert_eq!(model.provider, "openai");
                let ctx = crate::types::Context::default();
                provider.stream(&model, &ctx, None)
            };
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
    fn openai_completions_streams_uses_model_base_url_override() {
        let mut model = Model::new(
            "fixture-model",
            "Fixture model",
            "openai-completions",
            "fixture",
        );
        model.base_url = "http://model.example/v1".to_string();
        assert_eq!(
            resolve_openai_completions_base_url(&model, "http://fallback.example/v1"),
            "http://model.example/v1"
        );
        model.base_url.clear();
        assert_eq!(
            resolve_openai_completions_base_url(&model, "http://fallback.example/v1"),
            "http://fallback.example/v1"
        );
    }

    #[test]
    fn openai_responses_streams_uses_model_base_url_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind model URL fixture");
            let address = listener.local_addr().expect("model URL fixture address");
            let request_line = Arc::new(std::sync::Mutex::new(None));
            let request_line_server = Arc::clone(&request_line);
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("model URL request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = socket.read(&mut buffer).await.expect("read model URL request");
                    assert!(count > 0, "model URL client closed before headers");
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                *request_line_server.lock().unwrap_or_else(|error| error.into_inner()) = Some(line);

                let body = concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write model URL response");
            });

            let fallback = format!("http://{address}/fallback/v1");
            let mut model = Model::new("fixture-model", "Fixture model", "openai-responses", "fixture");
            model.base_url = format!("http://{address}/model/v1");
            let streams = openai_responses_streams(fallback);
            let mut options = crate::types::StreamOptions::default();
            options.base.api_key = Some("synthetic-key".to_string());
            let (events, message) = (streams.stream)(
                &model,
                &crate::types::Context::default(),
                Some(&options),
            )
            .collect()
            .await;

            server.await.expect("model URL fixture task");
            assert_eq!(message.stop_reason(), Some(crate::types::StopReason::Stop));
            assert!(events.iter().any(|event| matches!(
                event,
                crate::types::AssistantMessageEvent::TextDelta { delta, .. } if delta == "ok"
            )));
            assert_eq!(
                request_line.lock().unwrap_or_else(|error| error.into_inner()).as_deref(),
                Some("POST /model/v1/responses HTTP/1.1")
            );
        });
    }

    #[test]
    fn azure_provider_routes_through_azure_adaptor() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let stream = {
                let _guard = crate::utils::env_lock();
                std::env::remove_var("AZURE_OPENAI_API_KEY");
                let provider = azure_openai_responses_provider();
                let model = provider.models.first().cloned().unwrap();
                let ctx = crate::types::Context::default();
                provider.stream(&model, &ctx, None)
            };
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
            let stream = {
                let _guard = crate::utils::env_lock();
                std::env::remove_var("MISTRAL_API_KEY");
                let provider = mistral_provider();
                let model = provider.models.first().cloned().unwrap();
                assert_eq!(
                    model.api, "mistral-conversations",
                    "mistral catalog models must declare the mistral api"
                );
                let ctx = crate::types::Context::default();
                provider.stream(&model, &ctx, None)
            };
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
        // openai-codex must dispatch through the codex-responses adaptor. A
        // provider with no stored OAuth credential still reports an auth
        // failure rather than "no API implementation".
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
            assert!(provider.auth.oauth.is_some());
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
            let has_entry = streams.contains_key(&m.api);
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
        assert_eq!(all.len(), 1292);
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

    #[test]
    fn amazon_bedrock_auth_recognizes_ecs_and_web_identity_sources() {
        let provider = amazon_bedrock_provider();
        let auth = provider
            .auth
            .api_key
            .as_ref()
            .expect("Bedrock api-key auth");

        for (name, value, expected_source) in [
            (
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "/v2/credentials/123",
                "ECS task role",
            ),
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://169.254.170.2/credentials",
                "ECS task role",
            ),
            (
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                "/var/run/secrets/eks.amazonaws.com/serviceaccount/token",
                "web identity token",
            ),
        ] {
            let env = std::collections::BTreeMap::from([(name.to_string(), value.to_string())]);
            let ctx = crate::auth::AuthContext {
                env: Arc::new(move |key| env.get(key).cloned()),
                file_exists: Arc::new(|_: &str| false),
            };
            let check = auth
                .check(&ctx, None)
                .expect("auth source should be detected");
            assert_eq!(check.auth_type, "api_key");
            let resolved = auth
                .resolve(&ctx, None)
                .expect("auth source should resolve");
            assert_eq!(resolved.source.as_deref(), Some(expected_source));
        }
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
    fn google_vertex_provider_auth_uses_stored_adc_environment() {
        let explicit_path = "/tmp/pi-vertex-stored-adc.json".to_string();
        let stored_env = std::collections::BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                explicit_path.clone(),
            ),
            (
                "GOOGLE_CLOUD_PROJECT".to_string(),
                "stored-project".to_string(),
            ),
            (
                "GOOGLE_CLOUD_LOCATION".to_string(),
                "stored-location".to_string(),
            ),
        ]);
        let ambient_env = std::collections::BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                "/tmp/ambient-adc.json".to_string(),
            ),
            (
                "GOOGLE_CLOUD_PROJECT".to_string(),
                "ambient-project".to_string(),
            ),
            (
                "GOOGLE_CLOUD_LOCATION".to_string(),
                "ambient-location".to_string(),
            ),
        ]);
        let expected_path = explicit_path.clone();
        let ctx = crate::auth::AuthContext {
            env: Arc::new(move |name| ambient_env.get(name).cloned()),
            file_exists: Arc::new(move |path| path == expected_path),
        };
        let credential = crate::auth::ApiKeyCredential {
            key: None,
            env: Some(stored_env.clone()),
        };
        let auth = google_vertex_provider().auth.api_key.unwrap();
        let check = auth
            .check(&ctx, Some(&credential))
            .expect("stored ADC should be detected");
        assert_eq!(check.source.as_deref(), Some("stored credential"));
        let resolved = auth
            .resolve(&ctx, Some(&credential))
            .expect("stored ADC should resolve");
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));
        assert_eq!(resolved.env, Some(stored_env));
        assert!(resolved.auth.api_key.is_none());
    }

    #[test]
    fn google_vertex_provider_auth_requires_adc_project_and_location() {
        let env = std::collections::BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                "/tmp/pi-vertex-adc.json".to_string(),
            ),
            ("GOOGLE_CLOUD_PROJECT".to_string(), "project".to_string()),
        ]);
        let ctx = crate::auth::AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|path| path == "/tmp/pi-vertex-adc.json"),
        };
        let auth = google_vertex_provider().auth.api_key.unwrap();
        assert!(auth.check(&ctx, None).is_none());
    }

    #[test]
    fn google_vertex_provider_auth_prefers_ambient_api_key() {
        let env = std::collections::BTreeMap::from([(
            "GOOGLE_CLOUD_API_KEY".to_string(),
            "ambient-key".to_string(),
        )]);
        let ctx = crate::auth::AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|_| false),
        };
        let auth = google_vertex_provider().auth.api_key.unwrap();
        let resolved = auth
            .resolve(&ctx, None)
            .expect("ambient key should resolve");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("ambient-key"));
        assert_eq!(resolved.source.as_deref(), Some("GOOGLE_CLOUD_API_KEY"));
    }

    #[test]
    fn google_vertex_provider_auth_does_not_fallback_from_missing_explicit_adc() {
        let env = std::collections::BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                "/tmp/missing-explicit-adc.json".to_string(),
            ),
            ("HOME".to_string(), "/home/test".to_string()),
            ("GOOGLE_CLOUD_PROJECT".to_string(), "project".to_string()),
            ("GOOGLE_CLOUD_LOCATION".to_string(), "location".to_string()),
        ]);
        let ctx = crate::auth::AuthContext {
            env: Arc::new(move |name| env.get(name).cloned()),
            file_exists: Arc::new(|path| {
                path == crate::api::google_vertex::VERTEX_ADC_DEFAULT_PATH
            }),
        };
        let auth = google_vertex_provider().auth.api_key.unwrap();
        assert!(auth.check(&ctx, None).is_none());
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
            let has = provider.streams.contains_key(&m.api);
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
        assert_eq!(
            images.auth.api_key.as_ref().map(|auth| auth.name()),
            Some("OpenRouter API key")
        );
        assert_eq!(
            images.auth.oauth.as_ref().map(|auth| auth.name()),
            Some("OpenRouter OAuth")
        );
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
    fn opencode_provider_uses_upstream_names_auth_and_model_base_urls() {
        let opencode = opencode_provider();
        let opencode_go = opencode_go_provider();
        assert_eq!(opencode.name, "OpenCode Zen");
        assert_eq!(
            opencode.auth.api_key.as_ref().map(|auth| auth.name()),
            Some("OpenCode API key")
        );
        assert!(opencode.base_url.is_none());
        assert_eq!(opencode_go.name, "OpenCode Go");
        assert_eq!(
            opencode_go.auth.api_key.as_ref().map(|auth| auth.name()),
            Some("OpenCode API key")
        );
        assert!(opencode_go.base_url.is_none());

        let model = opencode
            .models
            .iter()
            .find(|model| model.api == "openai-completions")
            .expect("OpenCode completions model");
        assert_eq!(model.base_url, "https://opencode.ai/zen/v1");
        for api in [
            "anthropic-messages",
            "google-generative-ai",
            "openai-completions",
            "openai-responses",
        ] {
            assert!(opencode.streams.contains_key(api), "missing {api} stream");
        }
        for api in [
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
        ] {
            assert!(
                opencode_go.streams.contains_key(api),
                "missing {api} stream"
            );
        }
    }

    #[test]
    fn xai_provider_routes_the_current_catalog_through_responses_and_oauth() {
        let provider = xai_provider();
        assert_eq!(provider.name, "xAI");
        assert_eq!(provider.base_url.as_deref(), Some("https://api.x.ai/v1"));
        let model_ids: Vec<_> = provider
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(
            model_ids,
            vec!["grok-4.3", "grok-4.5", "grok-4.6", "grok-build-0.1"]
        );
        assert!(provider.models.iter().all(|model| {
            model.api == "openai-responses" && model.base_url == "https://api.x.ai/v1"
        }));
        assert!(provider.single_streams.is_some());
        assert!(provider.streams.is_empty());
        assert_eq!(
            provider.auth.api_key.as_ref().map(|auth| auth.name()),
            Some("xAI API key")
        );
        assert_eq!(
            provider.auth.oauth.as_ref().map(|auth| auth.name()),
            Some("xAI (Grok/X subscription)")
        );
        assert_eq!(
            provider
                .auth
                .oauth
                .as_ref()
                .and_then(|auth| auth.login_label()),
            Some("Sign in with SuperGrok or X Premium")
        );
    }

    #[test]
    fn together_and_vercel_use_upstream_auth_labels() {
        assert_eq!(
            together_provider()
                .auth
                .api_key
                .as_ref()
                .map(|auth| auth.name()),
            Some("Together API key")
        );
        assert_eq!(
            vercel_ai_gateway_provider()
                .auth
                .api_key
                .as_ref()
                .map(|auth| auth.name()),
            Some("Vercel AI Gateway API key")
        );
    }

    #[test]
    fn together_and_vercel_catalogs_match_pinned_model_surface() {
        let together = together_provider();
        assert_eq!(together.models.len(), 19);
        let together_model = together
            .models
            .iter()
            .find(|model| model.id == "deepseek-ai/DeepSeek-V4-Pro-0813")
            .expect("pinned Together model");
        assert_eq!(together_model.api, "openai-completions");
        assert_eq!(together_model.context_window, 1_048_576);
        assert_eq!(together_model.max_tokens, 384_000);
        assert_eq!(together_model.cost.input, 1.32);
        assert_eq!(together_model.cost.output, 3.96);

        let vercel = vercel_ai_gateway_provider();
        assert_eq!(vercel.models.len(), 222);
        let vercel_model = vercel
            .models
            .iter()
            .find(|model| model.id == "alibaba/qwen3.8-flash")
            .expect("pinned Vercel AI Gateway model");
        assert_eq!(vercel_model.api, "anthropic-messages");
        assert_eq!(vercel_model.input.len(), 2);
        assert_eq!(vercel_model.context_window, 991_000);
        assert_eq!(vercel_model.max_tokens, 128_000);
        assert_eq!(vercel_model.cost.input, 0.16);
        assert_eq!(vercel_model.cost.output, 0.47);
        assert!(vercel
            .models
            .iter()
            .all(|model| model.base_url == "https://ai-gateway.vercel.sh"));
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
    provider_with_env_auth_label(
        "google",
        "Google",
        "Gemini API key",
        Some(crate::api::google_generative_ai::DEFAULT_BASE_URL),
        &["GEMINI_API_KEY"],
        crate::models::ProviderApiSpec::Single(google_streams_from_model()),
    )
}

fn google_streams_from_model() -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let base_url = if model.base_url.trim().is_empty() {
                    crate::api::google_generative_ai::DEFAULT_BASE_URL
                } else {
                    model.base_url.as_str()
                };
                let go = crate::api::google_generative_ai::GoogleOptions::from_stream_options(
                    options.cloned().unwrap_or_default(),
                );
                crate::api::google_generative_ai::stream(
                    model,
                    ctx,
                    client.clone(),
                    base_url,
                    api_key,
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
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let base_url = if model.base_url.trim().is_empty() {
                    crate::api::google_generative_ai::DEFAULT_BASE_URL
                } else {
                    model.base_url.as_str()
                };
                let opts = options.cloned().unwrap_or_default();
                crate::api::google_generative_ai::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    base_url,
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
                let model_base_url = resolve_openai_responses_base_url(model, &base_url);
                let opts = crate::api::openai_responses::OpenAIResponsesOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::openai_responses::stream(
                    model,
                    ctx,
                    client.clone(),
                    model_base_url,
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
                let model_base_url = resolve_openai_responses_base_url(model, &base_url);
                let opts = options.cloned().unwrap_or_default();
                crate::api::openai_responses::stream_simple(
                    model,
                    ctx,
                    client.clone(),
                    model_base_url,
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

fn resolve_openai_responses_base_url<'a>(model: &'a Model, fallback: &'a str) -> &'a str {
    if model.base_url.trim().is_empty() {
        fallback
    } else {
        model.base_url.as_str()
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
                let mut anthropic_options =
                    crate::api::anthropic_messages::AnthropicOptions::default();
                if let Some(options) = options {
                    anthropic_options.base = options.clone();
                }
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &anthropic_options,
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
                let mut anthropic_options =
                    crate::api::anthropic_messages::AnthropicOptions::default();
                if let Some(options) = options {
                    anthropic_options.base = options.base.clone();
                }
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &base_url,
                    api_key,
                    &anthropic_options,
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
/// carry `{CLOUDFLARE_*}` placeholders materialized per-request). A fallback
/// base keeps hand-built models with an empty base URL compatible with the
/// fixed-base provider constructors.
fn openai_completions_streams_from_model_with_default(
    default_base_url: Option<&str>,
) -> crate::models::ProviderStreams {
    let client = reqwest::Client::new();
    let default_base_url = default_base_url.map(str::to_owned);
    let stream = {
        let client = client.clone();
        let default_base_url = default_base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::StreamOptions>| {
                let api_key = options.and_then(|o| o.base.api_key.as_deref());
                let base_url = if model.base_url.trim().is_empty() {
                    default_base_url.as_deref().unwrap_or("")
                } else {
                    &model.base_url
                };
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
                    base_url,
                    api_key,
                    &chat_options,
                )
            },
        )
    };
    let simple = {
        let client = client.clone();
        let default_base_url = default_base_url.clone();
        Arc::new(
            move |model: &Model,
                  ctx: &crate::types::Context,
                  options: Option<&crate::types::SimpleStreamOptions>| {
                let api_key = options.and_then(|o| o.base.base.api_key.as_deref());
                let base_url = if model.base_url.trim().is_empty() {
                    default_base_url.as_deref().unwrap_or("")
                } else {
                    &model.base_url
                };
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
                    base_url,
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

fn openai_completions_streams_from_model() -> crate::models::ProviderStreams {
    openai_completions_streams_from_model_with_default(None)
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
                let anthropic_options = crate::api::anthropic_messages::AnthropicOptions {
                    base: options.cloned().unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &anthropic_options,
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
                let anthropic_options = crate::api::anthropic_messages::AnthropicOptions {
                    base: options
                        .map(|options| options.base.clone())
                        .unwrap_or_default(),
                    ..Default::default()
                };
                crate::api::anthropic_messages::stream(
                    model,
                    ctx,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &anthropic_options,
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
/// token supplied in options; the coding-agent runtime resolves persisted
/// OAuth credentials into that request option before a turn.
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
