//! Radius gateway provider — port of `providers/radius.ts` and
//! `providers/radius-config.ts` from the upstream Pi checkout.
//!
//! Radius is intentionally dynamic.  Its provider has no bundled models;
//! `/v1/config` supplies the gateway catalog and `/messages` speaks the
//! `pi-messages` streaming protocol.  OAuth endpoints are also gateway-local,
//! which is important for custom Radius deployments and for deterministic
//! loopback tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::auth::{
    env_api_key_auth, AuthEvent, AuthInteraction, AuthPrompt, AuthSelectOption, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
use crate::model::{Model, ModelCost, ModelInput};
use crate::models::{
    create_provider, ModelsPersistence, ModelsPublication, ModelsStoreEntry, Provider,
    ProviderApiSpec, ProviderStreams, RefreshModelsContext, RefreshModelsFn,
};
use crate::oauth::{
    generate_pkce, poll_oauth_device_code_flow, DeviceCodePollOptions, DeviceCodePollResult,
};
use crate::types::{Context, SimpleStreamOptions, StreamOptions, ThinkingLevelMap};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/oauth/callback";
const OAUTH_CLIENT_ID: &str = "pi-gateway";
const OAUTH_SCOPE: &str = "gateway offline_access";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TOKEN_EXPIRY_SKEW_MS: u64 = 60_000;

/// A model entry returned by a Radius gateway's `/v1/config` endpoint.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// The dynamic catalog returned by a Radius gateway.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RadiusGatewayConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

/// An OAuth credential may contain a legacy, pre-ModelsStore Radius catalog.
pub type RadiusOAuthCredential = OAuthCredential;

fn sanitize_radius_gateway_config(value: Value) -> Option<RadiusGatewayConfig> {
    let object = value.as_object()?;
    let base_url = object.get("baseUrl")?.as_str()?.to_string();
    let models = object
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(|value| serde_json::from_value::<RadiusGatewayModel>(value.clone()).ok())
        .collect();
    Some(RadiusGatewayConfig { base_url, models })
}

/// Normalize a gateway URL exactly as upstream Radius does: add HTTPS when a
/// scheme is absent and remove trailing slashes.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let value = if value.starts_with("http://") || value.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    value.trim_end_matches('/').to_string()
}

/// Read a legacy catalog embedded in an OAuth credential, if present.
pub fn get_radius_credential_config(
    credential: Option<&OAuthCredential>,
) -> Option<RadiusGatewayConfig> {
    let value = credential?.extra.get("gatewayConfig")?.clone();
    sanitize_radius_gateway_config(value)
}

/// Convert a gateway config into the unified Rust model representation.
pub fn get_radius_models_from_config(
    provider_id: &str,
    config: &RadiusGatewayConfig,
) -> Vec<Model> {
    config
        .models
        .iter()
        .map(|model| Model {
            id: model.id.clone(),
            name: model.name.clone(),
            api: "pi-messages".to_string(),
            provider: provider_id.to_string(),
            base_url: config.base_url.clone(),
            reasoning: model.reasoning,
            thinking_level_map: model.thinking_level_map.clone(),
            input: model.input.clone(),
            cost: model.cost.clone(),
            context_window: model.context_window,
            max_tokens: model.max_tokens,
            sampling_params: None,
            headers: None,
            compat: None,
            authenticated: false,
            extra: model.extra.clone(),
        })
        .collect()
}

/// Convert a legacy OAuth credential's embedded catalog into models.
pub fn get_radius_models(provider_id: &str, credential: Option<&OAuthCredential>) -> Vec<Model> {
    get_radius_credential_config(credential)
        .map(|config| get_radius_models_from_config(provider_id, &config))
        .unwrap_or_default()
}

fn truncate_http_body(body: &str) -> String {
    let body = body.trim();
    if body.len() > 512 {
        format!("{}…", &body[..512])
    } else {
        body.to_string()
    }
}

async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn send_with_abort(
    request: reqwest::RequestBuilder,
    signal: Option<Arc<AtomicBool>>,
) -> Result<reqwest::Response, String> {
    let request = request.send();
    if let Some(signal) = signal {
        tokio::select! {
            response = request => response.map_err(|error| format!("request failed: {error}")),
            _ = wait_for_abort(signal) => Err("Request cancelled".to_string()),
        }
    } else {
        request
            .await
            .map_err(|error| format!("request failed: {error}"))
    }
}

/// Fetch and validate a Radius gateway catalog from `/v1/config`.
pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    signal: Option<Arc<AtomicBool>>,
) -> Result<RadiusGatewayConfig, String> {
    let gateway = normalize_radius_gateway_url(gateway);
    let url = format!("{gateway}/v1/config");
    let client = reqwest::Client::new();
    let mut request = client.get(&url).header("accept", "application/json");
    if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.bearer_auth(api_key);
    }
    let response = send_with_abort(request, signal).await?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read Radius config from {gateway}: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "Could not load Radius config from {gateway}: {}: {}",
            status.as_u16(),
            truncate_http_body(&body)
        ));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| format!("Invalid Radius config from {gateway}: {error}"))?;
    sanitize_radius_gateway_config(value)
        .ok_or_else(|| format!("Invalid Radius config from {gateway}"))
}

fn radius_streams() -> ProviderStreams {
    let client = reqwest::Client::new();
    let stream_client = client.clone();
    let stream = Arc::new(
        move |model: &Model, context: &Context, options: Option<&StreamOptions>| {
            let options = crate::api::pi_messages::PiMessagesOptions {
                base: options.cloned().unwrap_or_default(),
                ..Default::default()
            };
            let api_key = options.base.base.api_key.as_deref();
            crate::api::pi_messages::stream(
                model,
                context,
                stream_client.clone(),
                api_key,
                &options,
            )
        },
    );
    let simple_client = client;
    let stream_simple = Arc::new(
        move |model: &Model, context: &Context, options: Option<&SimpleStreamOptions>| {
            let options = options.cloned().unwrap_or_default();
            let api_key = options.base.base.api_key.as_deref();
            crate::api::pi_messages::stream_simple(
                model,
                context,
                simple_client.clone(),
                api_key,
                &options,
            )
        },
    );
    ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

fn credential_api_key(credential: Option<&crate::auth::Credential>) -> Option<String> {
    let value = match credential {
        Some(crate::auth::Credential::OAuth(credential)) => Some(credential.access.clone()),
        Some(crate::auth::Credential::ApiKey(credential)) => credential.key.clone(),
        None => None,
    };
    value
        .or_else(|| std::env::var("RADIUS_API_KEY").ok())
        .filter(|value| !value.trim().is_empty())
}

fn radius_refresh(
    provider_id: String,
    gateway: String,
    dynamic_models: Arc<std::sync::RwLock<Vec<Model>>>,
) -> RefreshModelsFn {
    Arc::new(move |context: RefreshModelsContext| {
        let provider_id = provider_id.clone();
        let gateway = gateway.clone();
        let dynamic_models = dynamic_models.clone();
        Box::pin(async move {
            let stored = context.stored.clone();
            if let Some(stored) = stored {
                let restored = stored
                    .models
                    .into_iter()
                    .filter(|model| model.provider == provider_id)
                    .collect::<Vec<_>>();
                let update_models = restored.clone();
                let state = dynamic_models.clone();
                if !context
                    .publish(ModelsPublication {
                        persist: None,
                        update: Some(Arc::new(move || {
                            *state.write().unwrap_or_else(|error| error.into_inner()) =
                                update_models.clone();
                        })),
                    })
                    .await?
                {
                    return Ok(());
                }
            }

            // Import catalogs cached by the legacy Radius credential format
            // before considering a network refresh.
            if context.stored.is_none() {
                if let Some(crate::auth::Credential::OAuth(credential)) =
                    context.credential.as_ref()
                {
                    let legacy = get_radius_models(&provider_id, Some(credential));
                    if !legacy.is_empty() {
                        let update_models = legacy.clone();
                        let state = dynamic_models.clone();
                        if !context
                            .publish(ModelsPublication {
                                persist: Some(ModelsPersistence::Write(ModelsStoreEntry {
                                    models: legacy,
                                    last_modified: None,
                                    checked_at: Some(crate::types::now_ms()),
                                    etag: None,
                                })),
                                update: Some(Arc::new(move || {
                                    *state.write().unwrap_or_else(|error| error.into_inner()) =
                                        update_models.clone();
                                })),
                            })
                            .await?
                        {
                            return Ok(());
                        }
                    }
                }
            }

            if !context.allow_network || context.aborted() {
                return Ok(());
            }
            let Some(api_key) = credential_api_key(context.credential.as_ref()) else {
                // The upstream Models facade skips unconfigured dynamic
                // providers before this phase.  Keep the same no-network,
                // empty-catalog behavior in the Rust facade as well.
                return Ok(());
            };
            let config =
                load_radius_gateway_config(&gateway, Some(&api_key), Some(context.signal.clone()))
                    .await?;
            if context.aborted() {
                return Ok(());
            }
            let refreshed = get_radius_models_from_config(&provider_id, &config);
            let update_models = refreshed.clone();
            let state = dynamic_models.clone();
            context
                .publish(ModelsPublication {
                    persist: Some(ModelsPersistence::Write(ModelsStoreEntry {
                        models: refreshed,
                        last_modified: None,
                        checked_at: Some(crate::types::now_ms()),
                        etag: None,
                    })),
                    update: Some(Arc::new(move || {
                        *state.write().unwrap_or_else(|error| error.into_inner()) =
                            update_models.clone();
                    })),
                })
                .await?;
            Ok(())
        })
    })
}

/// Options for the built-in Radius provider.
#[derive(Debug, Clone, Default)]
pub struct RadiusProviderOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub gateway: Option<String>,
}

/// Construct the dynamic Radius gateway provider.
pub fn radius_provider(options: RadiusProviderOptions) -> Provider {
    let id = options.id.unwrap_or_else(|| "radius".to_string());
    let name = options.name.unwrap_or_else(|| "Radius".to_string());
    let gateway =
        normalize_radius_gateway_url(options.gateway.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY));
    let dynamic_models = Arc::new(std::sync::RwLock::new(Vec::new()));
    let provider = create_provider(crate::models::CreateProviderOptions {
        id: id.clone(),
        name: Some(name.clone()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Radius API key",
                vec!["RADIUS_API_KEY".to_string()],
            )),
            oauth: Some(RadiusOAuth::new(name, gateway.clone())),
        },
        models: Vec::new(),
        api: ProviderApiSpec::Single(radius_streams()),
        filter_models: None,
    });
    provider.with_refresh_models_state(
        radius_refresh(id, gateway, dynamic_models.clone()),
        dynamic_models,
    )
}

#[derive(Debug, Clone)]
struct RadiusOAuthError {
    status: u16,
    oauth_error: Option<String>,
    description: Option<String>,
    message: String,
}

impl std::fmt::Display for RadiusOAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match (&self.oauth_error, &self.description) {
            (Some(error), Some(description)) => format!("{error}: {description}"),
            (Some(error), None) => error.clone(),
            (None, Some(description)) => description.clone(),
            (None, None) => self.status.to_string(),
        };
        write!(f, "{}: {detail}", self.message)
    }
}

impl std::error::Error for RadiusOAuthError {}

#[derive(Debug, Clone)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<f64>,
}

fn token_expiry(expires_in: u64) -> u64 {
    crate::types::now_ms()
        .saturating_add(expires_in.saturating_mul(1000))
        .saturating_sub(TOKEN_EXPIRY_SKEW_MS)
}

fn parse_oauth_error(status: u16, body: &str, message: &str) -> RadiusOAuthError {
    let (oauth_error, description) = if body.is_empty() {
        (None, None)
    } else {
        match serde_json::from_str::<Value>(body) {
            Ok(parsed) => (
                parsed
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parsed
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
            Err(_) => (None, Some(body.to_string())),
        }
    };
    RadiusOAuthError {
        status,
        oauth_error,
        description,
        message: message.to_string(),
    }
}

async fn post_form(
    client: &reqwest::Client,
    url: &str,
    form: &BTreeMap<&str, String>,
) -> Result<reqwest::Response, RadiusOAuthError> {
    let response = client
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .form(form)
        .send()
        .await
        .map_err(|error| RadiusOAuthError {
            status: 0,
            oauth_error: None,
            description: None,
            message: format!("request failed: {error}"),
        })?;
    if response.status().is_success() {
        Ok(response)
    } else {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Err(parse_oauth_error(
            status,
            &body,
            "Radius OAuth request failed",
        ))
    }
}

async fn request_device_authorization(
    gateway: &str,
) -> Result<DeviceAuthorizationResponse, String> {
    let mut form = BTreeMap::new();
    form.insert("client_id", OAUTH_CLIENT_ID.to_string());
    form.insert("scope", OAUTH_SCOPE.to_string());
    let response = post_form(
        &reqwest::Client::new(),
        &format!("{gateway}/v1/oauth/device"),
        &form,
    )
    .await
    .map_err(|error| error.to_string())?;
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("Invalid Radius OAuth device response: {error}"))?;
    let device_code = value
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Radius OAuth device authorization response is missing required fields".to_string()
        })?
        .to_string();
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Radius OAuth device authorization response is missing required fields".to_string()
        })?
        .to_string();
    let verification_uri = value
        .get("verification_uri")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Radius OAuth device authorization response is missing required fields".to_string()
        })?
        .to_string();
    let parsed_uri = url::Url::parse(&verification_uri)
        .map_err(|_| "Untrusted verification_uri in device code response".to_string())?;
    if !matches!(parsed_uri.scheme(), "http" | "https") {
        return Err("Untrusted verification_uri in device code response".to_string());
    }
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            "Radius OAuth device authorization response is missing required fields".to_string()
        })?;
    Ok(DeviceAuthorizationResponse {
        device_code,
        user_code,
        verification_uri: parsed_uri.to_string(),
        expires_in,
        interval: value.get("interval").and_then(Value::as_f64),
    })
}

async fn request_oauth_token(
    gateway: &str,
    form: BTreeMap<&str, String>,
) -> Result<OAuthCredential, RadiusOAuthError> {
    let response = post_form(
        &reqwest::Client::new(),
        &format!("{gateway}/v1/oauth/token"),
        &form,
    )
    .await?;
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| RadiusOAuthError {
            status: 200,
            oauth_error: None,
            description: None,
            message: format!("Invalid Radius OAuth token response: {error}"),
        })?;
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RadiusOAuthError {
            status: 200,
            oauth_error: None,
            description: None,
            message: "Radius OAuth token response is missing access_token".to_string(),
        })?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RadiusOAuthError {
            status: 200,
            oauth_error: None,
            description: None,
            message: "Radius OAuth token response is missing refresh_token".to_string(),
        })?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| RadiusOAuthError {
            status: 200,
            oauth_error: None,
            description: None,
            message: "Radius OAuth token response is missing expires_in".to_string(),
        })?;
    let mut extra = BTreeMap::new();
    if let Some(scope) = value.get("scope").and_then(Value::as_str) {
        extra.insert("scope".to_string(), Value::String(scope.to_string()));
    }
    Ok(OAuthCredential {
        refresh: refresh.to_string(),
        access: access.to_string(),
        expires: token_expiry(expires_in),
        extra,
    })
}

async fn load_oauth_discovery(gateway: &str) -> Result<String, String> {
    let response = reqwest::Client::new()
        .get(format!("{gateway}/v1/oauth"))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read Radius OAuth config: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "Could not load Radius OAuth config from {gateway}: {} {}",
            status.as_u16(),
            body
        ));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| format!("Invalid Radius OAuth config from {gateway}: {error}"))?;
    let endpoint = value
        .get("authorizationEndpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Invalid Radius OAuth config from {gateway}"))?;
    let parsed = url::Url::parse(endpoint)
        .map_err(|_| "Radius OAuth authorization endpoint is not a valid URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Radius OAuth authorization endpoint is not an HTTP URL".to_string());
    }
    Ok(parsed.to_string())
}

struct CallbackServer {
    receiver: tokio::sync::oneshot::Receiver<Result<String, String>>,
    task: tokio::task::JoinHandle<()>,
    port: u16,
}

async fn start_callback_server(
    expected_state: String,
    requested_port: u16,
) -> Result<CallbackServer, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind((CALLBACK_HOST, requested_port))
        .await
        .map_err(|error| format!("Radius OAuth callback server failed to bind: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Radius OAuth callback address failed: {error}"))?
        .port();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut sender = Some(sender);
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if buffer.len() > 32 * 1024 {
                    return;
                }
            }
            let request_line = String::from_utf8_lossy(&buffer)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let target = request_line.split_whitespace().nth(1).unwrap_or("/");
            let callback_url = url::Url::parse(&format!("http://{CALLBACK_HOST}:{port}{target}"));
            let Ok(callback_url) = callback_url else {
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            };
            if callback_url.path() != CALLBACK_PATH {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            if callback_url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                != Some(expected_state.clone())
            {
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            if let Some(error) = callback_url
                .query_pairs()
                .find(|(key, _)| key == "error")
                .map(|(_, value)| value.into_owned())
            {
                let body = format!("Radius OAuth failed: {error}");
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                if let Some(sender) = sender.take() {
                    let _ = sender.send(Err(body));
                }
                return;
            }
            let Some(code) = callback_url
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.into_owned())
            else {
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            };
            let body = "Signed in to Radius. You may now close this page.";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            if let Some(sender) = sender.take() {
                let _ = sender.send(Ok(code));
            }
            return;
        }
    });
    Ok(CallbackServer {
        receiver,
        task,
        port,
    })
}

/// Radius OAuth implementation.  The production provider uses the upstream
/// fixed callback port; `with_callback_port(0)` is public so loopback tests
/// can obtain an ephemeral port without global test-state or network access.
pub struct RadiusOAuth {
    name: String,
    gateway: String,
    callback_port: u16,
}

impl RadiusOAuth {
    pub fn new(name: impl Into<String>, gateway: impl AsRef<str>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            gateway: normalize_radius_gateway_url(gateway.as_ref()),
            callback_port: CALLBACK_PORT,
        })
    }

    pub fn with_callback_port(
        name: impl Into<String>,
        gateway: impl AsRef<str>,
        callback_port: u16,
    ) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            gateway: normalize_radius_gateway_url(gateway.as_ref()),
            callback_port,
        })
    }

    async fn login_device(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, String> {
        let device = request_device_authorization(&self.gateway).await?;
        interaction.notify(&AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
        });
        let gateway = self.gateway.clone();
        let device_code = device.device_code.clone();
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            let gateway = gateway.clone();
            let device_code = device_code.clone();
            Box::pin(async move {
                let mut form = BTreeMap::new();
                form.insert("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE.to_string());
                form.insert("client_id", OAUTH_CLIENT_ID.to_string());
                form.insert("device_code", device_code);
                match request_oauth_token(&gateway, form).await {
                    Ok(credential) => DeviceCodePollResult::Complete(credential),
                    Err(error) => match error.oauth_error.as_deref() {
                        Some("authorization_pending") => DeviceCodePollResult::Pending,
                        Some("slow_down") => DeviceCodePollResult::SlowDown {
                            interval_seconds: None,
                        },
                        Some("expired_token") => DeviceCodePollResult::Failed {
                            message: "Device authorization expired.".to_string(),
                        },
                        Some("access_denied") => DeviceCodePollResult::Failed {
                            message: "Device authorization was denied.".to_string(),
                        },
                        _ => DeviceCodePollResult::Failed {
                            message: error.to_string(),
                        },
                    },
                }
            })
        }));
        options.interval_seconds = device.interval;
        options.expires_in_seconds = Some(device.expires_in);
        poll_oauth_device_code_flow(&mut options).await
    }

    async fn login_browser(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, String> {
        let authorization_endpoint = load_oauth_discovery(&self.gateway).await?;
        let (verifier, challenge) = generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();
        let callback = start_callback_server(state.clone(), self.callback_port).await?;
        let redirect_uri = format!("http://{CALLBACK_HOST}:{}{}", callback.port, CALLBACK_PATH);
        let mut authorize_url = url::Url::parse(&authorization_endpoint)
            .map_err(|error| format!("Invalid Radius OAuth authorization endpoint: {error}"))?;
        {
            let mut query = authorize_url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", OAUTH_CLIENT_ID);
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("scope", OAUTH_SCOPE);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            query.append_pair("handoff", "url");
            query.append_pair("state", &state);
        }
        interaction.notify(&AuthEvent::Progress {
            message: format!("Listening for OAuth callback on {redirect_uri}"),
        });
        interaction.notify(&AuthEvent::AuthUrl {
            url: authorize_url.to_string(),
            instructions: Some("Continue in your browser.".to_string()),
        });
        let code = match callback.receiver.await {
            Ok(result) => result?,
            Err(_) => return Err("OAuth callback did not complete.".to_string()),
        };
        callback.task.abort();
        let mut form = BTreeMap::new();
        form.insert("grant_type", "authorization_code".to_string());
        form.insert("client_id", OAUTH_CLIENT_ID.to_string());
        form.insert("redirect_uri", redirect_uri);
        form.insert("code", code);
        form.insert("code_verifier", verifier);
        request_oauth_token(&self.gateway, form)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> bool {
        false
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let method = interaction.prompt(&AuthPrompt::Select {
            message: format!("Sign in to {}:", self.name),
            options: vec![
                AuthSelectOption {
                    id: "browser".to_string(),
                    label: "Sign in with browser (recommended)".to_string(),
                    description: None,
                },
                AuthSelectOption {
                    id: "device-code".to_string(),
                    label: "Sign in with device code (when signing in from another device)"
                        .to_string(),
                    description: None,
                },
            ],
        })?;
        match method.as_str() {
            "browser" => self.login_browser(interaction).await,
            "device-code" => self.login_device(interaction).await,
            other => Err(format!("Unknown {} sign-in method: {other}", self.name)),
        }
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        if signal.load(Ordering::SeqCst) {
            return Err("Login cancelled".to_string());
        }
        let mut form = BTreeMap::new();
        form.insert("grant_type", "refresh_token".to_string());
        form.insert("client_id", OAUTH_CLIENT_ID.to_string());
        form.insert("refresh_token", credential.refresh.clone());
        let result = request_oauth_token(&self.gateway, form)
            .await
            .map_err(|error| error.to_string())?;
        if signal.load(Ordering::SeqCst) {
            return Err("Login cancelled".to_string());
        }
        Ok(result)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: None,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::types::ModelThinkingLevel;

    #[test]
    fn normalizes_gateway_and_preserves_dynamic_model_fields() {
        assert_eq!(
            normalize_radius_gateway_url("radius.example/"),
            "https://radius.example"
        );
        assert_eq!(
            normalize_radius_gateway_url("http://127.0.0.1:8788///"),
            "http://127.0.0.1:8788"
        );
        let config = RadiusGatewayConfig {
            base_url: "http://gateway/v1".to_string(),
            models: vec![RadiusGatewayModel {
                id: "auto".to_string(),
                name: "Radius Auto".to_string(),
                reasoning: true,
                thinking_level_map: Some(BTreeMap::from([(
                    ModelThinkingLevel::High,
                    Some("high".to_string()),
                )])),
                input: vec![ModelInput::Text],
                cost: ModelCost::default(),
                context_window: 128_000,
                max_tokens: 16_384,
                extra: BTreeMap::from([("routing".to_string(), json!("fast"))]),
            }],
        };
        let model = get_radius_models_from_config("radius", &config)
            .into_iter()
            .next()
            .expect("dynamic model");
        assert_eq!(model.api, "pi-messages");
        assert_eq!(model.base_url, "http://gateway/v1");
        assert_eq!(model.extra.get("routing"), Some(&json!("fast")));
        assert_eq!(
            model.thinking_level_map,
            config.models[0].thinking_level_map
        );
    }

    #[test]
    fn legacy_catalog_is_read_from_oauth_extra() {
        let config = json!({
            "baseUrl": "http://gateway/v1",
            "models": [{
                "id": "auto",
                "name": "Radius Auto",
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2},
                "contextWindow": 128000,
                "maxTokens": 16384
            }]
        });
        let credential = OAuthCredential {
            refresh: "refresh".to_string(),
            access: "access".to_string(),
            expires: u64::MAX,
            extra: BTreeMap::from([("gatewayConfig".to_string(), config)]),
        };
        let models = get_radius_models("radius", Some(&credential));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "auto");
        assert_eq!(models[0].base_url, "http://gateway/v1");
    }

    #[test]
    fn oauth_errors_preserve_upstream_detail_order_and_plain_body() {
        let structured = parse_oauth_error(
            400,
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
            "Radius OAuth token request failed",
        );
        assert_eq!(
            structured.to_string(),
            "Radius OAuth token request failed: invalid_grant: expired"
        );

        let malformed = parse_oauth_error(
            502,
            "gateway unavailable",
            "Radius OAuth device authorization failed",
        );
        assert_eq!(
            malformed.to_string(),
            "Radius OAuth device authorization failed: gateway unavailable"
        );

        let empty = parse_oauth_error(503, "", "Radius OAuth request failed");
        assert_eq!(empty.to_string(), "Radius OAuth request failed: 503");
    }
}
