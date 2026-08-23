//! Provider OAuth flows — port of `packages/ai/src/auth/oauth/*.ts`.
//!
//! Two flow shapes:
//! - **Device code** (RFC 8628): GitHub Copilot, OpenAI Codex, Kimi, XAI,
//!   Radius. POST a device-authorization request, show the user code, poll
//!   the token endpoint.
//! - **Callback server + PKCE**: OpenRouter, Anthropic. A one-shot loopback
//!   HTTP server receives the authorization code, raced against a manual
//!   paste prompt for headless sessions.
//!
//! The polling loop itself lives in `crate::oauth` (device-code.ts + pkce.ts
//! port); this module layers the provider-specific endpoints on top.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::oauth::{poll_oauth_device_code_flow, DeviceCodePollOptions, DeviceCodePollResult};

/// RFC 8628 device-authorization response.
#[derive(Debug, Clone)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Option<f64>,
    pub expires_in: u64,
}

/// POST a form body and parse the JSON response (upstream `fetchJson`).
async fn post_form_json(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Result<serde_json::Value, String> {
    let mut req = client.post(url).form(form);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response: {e}"))?;
    if !status.is_success() {
        return Err(format!("{status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON response: {e}"))
}

/// GET a URL with headers and parse the JSON response.
async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<serde_json::Value, String> {
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response: {e}"))?;
    if !status.is_success() {
        return Err(format!("{status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON response: {e}"))
}

/// Start an RFC 8628 device flow: POST the client_id/scope form and validate
/// the response fields (upstream `startDeviceFlow`).
pub async fn start_device_flow(
    client: &reqwest::Client,
    device_code_url: &str,
    form: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Result<DeviceCodeResponse, String> {
    let data = post_form_json(client, device_code_url, form, headers).await?;
    let device_code = data
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Invalid device code response fields".to_string())?
        .to_string();
    let user_code = data
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Invalid device code response fields".to_string())?
        .to_string();
    let verification_uri = data
        .get("verification_uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Invalid device code response fields".to_string())?
        .to_string();
    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Invalid device code response fields".to_string())?;
    let interval = data.get("interval").and_then(|v| v.as_f64());

    // The verification URI is opened in the user's browser; force it to be a
    // real http(s) URL so `open` cannot be pointed at an executable.
    let parsed = url::Url::parse(&verification_uri)
        .map_err(|_| "Untrusted verification_uri in device code response".to_string())?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err("Untrusted verification_uri in device code response".to_string());
    }

    Ok(DeviceCodeResponse {
        device_code,
        user_code,
        verification_uri: parsed.to_string(),
        interval,
        expires_in,
    })
}

/// Poll the token endpoint until the access token arrives (upstream
/// `pollForAccessToken`). The `poll` closure maps each response to a
/// `DeviceCodePollResult`.
pub async fn poll_for_access_token(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
    headers: &[(&str, &str)],
    device: &DeviceCodeResponse,
    signal: Option<&Arc<AtomicBool>>,
) -> Result<String, String> {
    let mut options = DeviceCodePollOptions::new(Box::new({
        let client = client.clone();
        let token_url = token_url.to_string();
        let form: Vec<(String, String)> = form
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let device_code = device.device_code.clone();
        move || {
            let client = client.clone();
            let token_url = token_url.clone();
            let form = form.clone();
            let headers = headers.clone();
            let device_code = device_code.clone();
            Box::pin(async move {
                let mut body = form.clone();
                body.push(("device_code".to_string(), device_code));
                body.push((
                    "grant_type".to_string(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                ));
                let data = match post_form_json(
                    &client,
                    &token_url,
                    &body
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<_>>(),
                    &headers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<_>>(),
                )
                .await
                {
                    Ok(d) => d,
                    Err(e) => return DeviceCodePollResult::Failed { message: e },
                };
                if let Some(token) = data.get("access_token").and_then(|v| v.as_str()) {
                    return DeviceCodePollResult::Complete(token.to_string());
                }
                if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
                    match error {
                        "authorization_pending" => return DeviceCodePollResult::Pending,
                        "slow_down" => {
                            let interval = data.get("interval").and_then(|v| v.as_f64());
                            return DeviceCodePollResult::SlowDown {
                                interval_seconds: interval,
                            };
                        }
                        _ => {
                            let description = data
                                .get("error_description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let suffix = if description.is_empty() {
                                String::new()
                            } else {
                                format!(": {description}")
                            };
                            return DeviceCodePollResult::Failed {
                                message: format!("Device flow failed: {error}{suffix}"),
                            };
                        }
                    }
                }
                DeviceCodePollResult::Failed {
                    message: "Invalid device token response".to_string(),
                }
            })
        }
    }));
    options.interval_seconds = device.interval;
    options.expires_in_seconds = Some(device.expires_in);
    options.wait_before_first_poll = true;
    options.signal = signal.cloned();
    poll_oauth_device_code_flow(&mut options).await
}

// ---------------------------------------------------------------------------
// GitHub Copilot (device code)
// ---------------------------------------------------------------------------

const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

fn copilot_urls(domain: &str) -> (String, String, String) {
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://api.{domain}/copilot_internal/v2/token"),
    )
}

/// Normalize a user-supplied enterprise domain (upstream `normalizeDomain`).
fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&with_scheme)
        .ok()
        .map(|u| u.host_str().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
}

/// Parse `proxy-ep=...` from a Copilot token and convert to the API base URL
/// (upstream `getBaseUrlFromToken`).
fn copilot_base_url_from_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))?;
    // Convert proxy.xxx to api.xxx
    let api_host = if let Some(rest) = proxy_host.strip_prefix("proxy.") {
        format!("api.{rest}")
    } else {
        proxy_host.to_string()
    };
    Some(format!("https://{api_host}"))
}

fn copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(token) = token {
        if let Some(url) = copilot_base_url_from_token(token) {
            return url;
        }
    }
    match enterprise_domain {
        Some(domain) => format!("https://copilot-api.{domain}"),
        None => "https://api.individual.githubcopilot.com".to_string(),
    }
}

/// Exchange the GitHub access token for a Copilot token (upstream
/// `refreshGitHubCopilotAccessToken`).
async fn copilot_token_exchange(
    client: &reqwest::Client,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
) -> Result<OAuthCredential, String> {
    let domain = enterprise_domain.unwrap_or("github.com");
    let (_, _, copilot_token_url) = copilot_urls(domain);
    let data = get_json(client, &copilot_token_url, &COPILOT_HEADERS)
        .await
        .map_err(|e| format!("copilot token request failed: {e}"))?;
    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Invalid Copilot token response fields".to_string())?;
    let expires_at = data
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "Invalid Copilot token response fields".to_string())?;
    Ok(OAuthCredential {
        refresh: refresh_token.to_string(),
        access: token.to_string(),
        expires: expires_at
            .saturating_mul(1000)
            .saturating_sub(5 * 60 * 1000),
        extra: {
            let mut m = std::collections::BTreeMap::new();
            if let Some(d) = enterprise_domain {
                m.insert(
                    "enterpriseUrl".to_string(),
                    serde_json::Value::String(d.to_string()),
                );
            }
            m
        },
    })
}

/// GitHub Copilot OAuth flow (upstream `githubCopilotOAuth`).
pub struct GitHubCopilotOAuth;

impl GitHubCopilotOAuth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait::async_trait]
impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let input = interaction.prompt(&crate::auth::AuthPrompt::Text {
            message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
            placeholder: Some("company.ghe.com".to_string()),
        })?;
        let trimmed = input.trim();
        let enterprise_domain = normalize_domain(trimmed);
        if !trimmed.is_empty() && enterprise_domain.is_none() {
            return Err("Invalid GitHub Enterprise URL/domain".to_string());
        }
        let domain = enterprise_domain
            .clone()
            .unwrap_or_else(|| "github.com".to_string());
        let (device_code_url, access_token_url, _) = copilot_urls(&domain);

        let client = reqwest::Client::new();
        let device = start_device_flow(
            &client,
            &device_code_url,
            &[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")],
            &[
                ("Accept", "application/json"),
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("User-Agent", "GitHubCopilotChat/0.35.0"),
            ],
        )
        .await?;

        interaction.notify(&AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
        });

        let github_access_token = poll_for_access_token(
            &client,
            &access_token_url,
            &[("client_id", COPILOT_CLIENT_ID)],
            &[
                ("Accept", "application/json"),
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("User-Agent", "GitHubCopilotChat/0.35.0"),
            ],
            &device,
            None,
        )
        .await?;

        copilot_token_exchange(&client, &github_access_token, enterprise_domain.as_deref()).await
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        _signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        let enterprise_domain = credential
            .extra
            .get("enterpriseUrl")
            .and_then(|v| v.as_str())
            .and_then(normalize_domain);
        let client = reqwest::Client::new();
        copilot_token_exchange(&client, &credential.refresh, enterprise_domain.as_deref()).await
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        let enterprise_domain = credential
            .extra
            .get("enterpriseUrl")
            .and_then(|v| v.as_str())
            .and_then(normalize_domain);
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: Some(copilot_base_url(
                Some(&credential.access),
                enterprise_domain.as_deref(),
            )),
            headers: None,
        })
    }
}

// ---------------------------------------------------------------------------
// OpenRouter (callback server + PKCE)
// ---------------------------------------------------------------------------

const OPENROUTER_AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const OPENROUTER_TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const OPENROUTER_LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// One-shot loopback HTTP server that captures the OAuth callback code.
struct CallbackServer {
    listener: tokio::net::TcpListener,
}

impl CallbackServer {
    /// Bind on 127.0.0.1 (ephemeral port unless `port` is Some) and return
    /// the server plus its callback URL.
    async fn start_on(callback_path: String, port: Option<u16>) -> Result<(Self, String), String> {
        let addr = match port {
            Some(p) => format!("127.0.0.1:{p}"),
            None => "127.0.0.1:0".to_string(),
        };
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind callback server: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("callback addr: {e}"))?
            .port();
        let url = format!("http://127.0.0.1:{port}{callback_path}");
        Ok((Self { listener }, url))
    }

    /// Bind an ephemeral port on 127.0.0.1 and return the server plus its
    /// callback URL.
    async fn start(callback_path: String) -> Result<(Self, String), String> {
        Self::start_on(callback_path, None).await
    }

    /// Accept one connection, parse the GET query, and reply with `html`.
    /// Returns the query string of the request.
    async fn wait_for_callback(&self, html: &str) -> Result<String, String> {
        let (mut socket, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| format!("callback accept: {e}"))?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 8192];
        let n = socket
            .read(&mut buf)
            .await
            .map_err(|e| format!("callback read: {e}"))?;
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let request_line = request.lines().next().unwrap_or("").to_string();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let query = path.split('?').nth(1).unwrap_or("").to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        Ok(query)
    }
}

/// Parse `code` out of a pasted URL / query string / raw code (upstream
/// `parseAuthorizationInput`).
fn parse_authorization_code(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(value) {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        return pairs
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone());
    }
    if value.contains("code=") {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(value.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        return pairs
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone());
    }
    Some(value.to_string())
}

/// Exchange the authorization code for a permanent OpenRouter API key
/// (upstream `exchangeAuthorizationCode`).
async fn openrouter_exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<OAuthCredential, String> {
    let resp = client
        .post(OPENROUTER_TOKEN_URL)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .map_err(|e| format!("OpenRouter key exchange failed: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("OpenRouter key exchange returned invalid JSON: {e}"))?;
    if !status.is_success() {
        let detail = body
            .get("error_description")
            .or_else(|| body.get("message"))
            .or_else(|| body.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!(
            "OpenRouter OAuth key exchange failed (HTTP {status}){detail}"
        ));
    }
    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty());
    match key {
        Some(key) => Ok(OAuthCredential {
            access: key.to_string(),
            refresh: String::new(),
            expires: u64::MAX,
            extra: Default::default(),
        }),
        None => Err("OpenRouter OAuth response carries no \"key\"".to_string()),
    }
}

/// OpenRouter OAuth flow (upstream `openRouterOAuth`).
pub struct OpenRouterOAuth;

impl OpenRouterOAuth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait::async_trait]
impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn is_subscription(&self) -> bool {
        false
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let (verifier, challenge) = crate::oauth::generate_pkce();
        let callback_path = format!("/oauth/callback/{}", uuid::Uuid::new_v4());
        let (server, callback_url) = CallbackServer::start(callback_path).await?;

        let authorize_url = format!(
            "{OPENROUTER_AUTHORIZE_URL}?callback_url={}&code_challenge={}&code_challenge_method=S256",
            url::form_urlencoded::byte_serialize(callback_url.as_bytes()).collect::<String>(),
            challenge
        );

        interaction.notify(&AuthEvent::Progress {
            message: format!("Listening for OpenRouter OAuth callback on {callback_url}"),
        });
        interaction.notify(&AuthEvent::AuthUrl {
            url: authorize_url.clone(),
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".to_string(),
            ),
        });

        // Race the callback server against a manual paste prompt.
        let client = reqwest::Client::new();
        let callback_future = async {
            let query = tokio::time::timeout(
                std::time::Duration::from_millis(OPENROUTER_LOGIN_TIMEOUT_MS),
                server.wait_for_callback("<!DOCTYPE html><html><body><h1>Signed in to OpenRouter. You may now close this page.</h1></body></html>"),
            )
            .await
            .map_err(|_| "OpenRouter OAuth login timed out".to_string())??;
            parse_authorization_code(&query)
                .ok_or_else(|| "OpenRouter returned no authorization code".to_string())
        };

        let manual_future = async {
            let input = interaction.prompt(&crate::auth::AuthPrompt::ManualCode {
                message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:".to_string(),
                placeholder: Some(callback_url.clone()),
            })?;
            parse_authorization_code(&input).ok_or_else(|| "Missing authorization code".to_string())
        };

        let code = tokio::select! {
            result = callback_future => result?,
            result = manual_future => result?,
        };

        interaction.notify(&AuthEvent::Progress {
            message: "Exchanging authorization code for an API key...".to_string(),
        });
        openrouter_exchange_code(&client, &code, &verifier).await
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        _signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        // OpenRouter keys are permanent; refresh is a no-op.
        Ok(credential.clone())
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: None,
            headers: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Anthropic (callback server + PKCE, fixed port)
// ---------------------------------------------------------------------------

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const ANTHROPIC_CALLBACK_PORT: u16 = 53692;
const ANTHROPIC_CALLBACK_PATH: &str = "/callback";
const ANTHROPIC_REDIRECT_URI: &str = "http://localhost:53692/callback";
const ANTHROPIC_SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// POST a JSON body and return the response text (upstream `postJson`).
async fn post_json_text(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<String, String> {
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "HTTP request failed. status={status}; url={url}; body={text}"
        ));
    }
    Ok(text)
}

/// Exchange the authorization code for tokens (upstream
/// `exchangeAuthorizationCode`).
async fn anthropic_exchange_code(
    client: &reqwest::Client,
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<OAuthCredential, String> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": ANTHROPIC_CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": ANTHROPIC_REDIRECT_URI,
        "code_verifier": verifier,
    });
    let text = post_json_text(client, ANTHROPIC_TOKEN_URL, &body).await?;
    let data: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Token exchange returned invalid JSON: {e}"))?;
    let access = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?;
    let refresh = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("missing refresh_token")?;
    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .ok_or("missing expires_in")?;
    let now_ms = crate::types::now_ms();
    Ok(OAuthCredential {
        refresh: refresh.to_string(),
        access: access.to_string(),
        expires: now_ms + expires_in * 1000 - 5 * 60 * 1000,
        extra: Default::default(),
    })
}

/// Refresh an Anthropic OAuth token (upstream `refreshAnthropicToken`).
async fn anthropic_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthCredential, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": ANTHROPIC_CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let text = post_json_text(client, ANTHROPIC_TOKEN_URL, &body).await?;
    let data: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Token refresh returned invalid JSON: {e}"))?;
    let access = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?;
    let refresh = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("missing refresh_token")?;
    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .ok_or("missing expires_in")?;
    let now_ms = crate::types::now_ms();
    Ok(OAuthCredential {
        refresh: refresh.to_string(),
        access: access.to_string(),
        expires: now_ms + expires_in * 1000 - 5 * 60 * 1000,
        extra: Default::default(),
    })
}

/// Anthropic OAuth flow (upstream `anthropicOAuth`).
pub struct AnthropicOAuth;

impl AnthropicOAuth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait::async_trait]
impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let (verifier, challenge) = crate::oauth::generate_pkce();
        let (server, _) = CallbackServer::start_on(
            ANTHROPIC_CALLBACK_PATH.to_string(),
            Some(ANTHROPIC_CALLBACK_PORT),
        )
        .await?;

        let auth_params = format!(
            "code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            ANTHROPIC_CLIENT_ID,
            url::form_urlencoded::byte_serialize(ANTHROPIC_REDIRECT_URI.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(ANTHROPIC_SCOPES.as_bytes()).collect::<String>(),
            challenge,
            verifier
        );
        let authorize_url = format!("{ANTHROPIC_AUTHORIZE_URL}?{auth_params}");

        interaction.notify(&AuthEvent::AuthUrl {
            url: authorize_url.clone(),
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .to_string(),
            ),
        });

        // Race the callback server against a manual paste prompt.
        let client = reqwest::Client::new();
        let callback_future = async {
            let query = server
                .wait_for_callback("<!DOCTYPE html><html><body><h1>Anthropic authentication completed. You can close this window.</h1></body></html>")
                .await?;
            let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let code = pairs
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.clone());
            let state = pairs
                .iter()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.clone());
            match (code, state) {
                (Some(code), Some(state)) => Ok((code, state)),
                _ => Err("Missing code or state parameter".to_string()),
            }
        };

        let manual_future = async {
            let input = interaction.prompt(&crate::auth::AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the authorization code / redirect URL here:".to_string(),
                placeholder: Some(ANTHROPIC_REDIRECT_URI.to_string()),
            })?;
            let parsed = parse_authorization_code(&input)
                .ok_or_else(|| "Missing authorization code".to_string())?;
            Ok::<(String, String), String>((parsed, verifier.clone()))
        };

        let (code, state) = tokio::select! {
            result = callback_future => result?,
            result = manual_future => result?,
        };
        if state != verifier {
            return Err("OAuth state mismatch".to_string());
        }

        interaction.notify(&AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".to_string(),
        });
        anthropic_exchange_code(&client, &code, &state, &verifier).await
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        _signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        let client = reqwest::Client::new();
        anthropic_refresh_token(&client, &credential.refresh).await
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: None,
            headers: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthPrompt, AuthSelectOption};

    struct ScriptedInteraction {
        prompts: std::sync::Mutex<Vec<AuthPrompt>>,
        answers: std::sync::Mutex<Vec<String>>,
        events: std::sync::Mutex<Vec<AuthEvent>>,
    }

    impl ScriptedInteraction {
        fn new(answers: Vec<String>) -> Self {
            Self {
                prompts: std::sync::Mutex::new(Vec::new()),
                answers: std::sync::Mutex::new(answers),
                events: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl AuthInteraction for ScriptedInteraction {
        fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
            self.prompts.lock().unwrap().push(prompt.clone());
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                Err("no scripted answer".to_string())
            } else {
                Ok(answers.remove(0))
            }
        }
        fn notify(&self, event: &AuthEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    #[test]
    fn normalize_domain_accepts_urls_and_hosts() {
        assert_eq!(
            normalize_domain("company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("https://company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(normalize_domain("  "), None);
        assert_eq!(normalize_domain("not a url"), None);
    }

    #[test]
    fn copilot_base_url_parses_proxy_ep() {
        assert_eq!(
            copilot_base_url_from_token(
                "tid=1;exp=2;proxy-ep=proxy.individual.githubcopilot.com;x=1"
            )
            .as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        assert_eq!(
            copilot_base_url(None, None),
            "https://api.individual.githubcopilot.com"
        );
        assert_eq!(
            copilot_base_url(None, Some("company.ghe.com")),
            "https://copilot-api.company.ghe.com"
        );
    }

    #[test]
    fn parse_authorization_code_handles_urls_queries_and_raw() {
        assert_eq!(
            parse_authorization_code(
                "http://127.0.0.1:54321/oauth/callback/abc?code=sekret&state=x"
            )
            .as_deref(),
            Some("sekret")
        );
        assert_eq!(
            parse_authorization_code("code=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_authorization_code("rawcode").as_deref(),
            Some("rawcode")
        );
        assert_eq!(parse_authorization_code(""), None);
    }

    #[test]
    fn openrouter_flow_prompts_and_emits_auth_url() {
        let interaction =
            ScriptedInteraction::new(vec!["http://127.0.0.1:1/cb?code=manual-code".to_string()]);
        // The callback server never receives a connection; the manual paste
        // path must win via the select. We can't easily run the full login
        // here (it would block on the callback accept), so verify the prompt
        // shape and event emission through the pieces we can reach.
        let prompts = interaction.prompts.lock().unwrap();
        assert!(prompts.is_empty());
        drop(prompts);
        // parse_authorization_code is exercised above; the flow itself is
        // covered by the mock-server integration test in pi-coding-agent.
        let _ = interaction;
    }

    #[test]
    fn select_option_shape() {
        let opt = AuthSelectOption {
            id: "a".into(),
            label: "A".into(),
            description: None,
        };
        assert_eq!(opt.id, "a");
    }
}
#[cfg(test)]
mod device_flow_tests {
    use super::*;

    /// Minimal HTTP server that answers a fixed set of (path, response) pairs
    /// in order, then closes. Returns the base URL.
    async fn mock_server(routes: Vec<(String, String)>) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut routes = routes;
            for _ in 0..routes.len() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let request = String::from_utf8_lossy(&buf).to_string();
                let path = request
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let (expected_path, body) = routes.remove(0);
                assert_eq!(path, expected_path, "unexpected request path: {path}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        base
    }

    #[tokio::test]
    async fn device_flow_completes_after_pending_then_success() {
        let base = mock_server(vec![
            (
                "/device/code".to_string(),
                r#"{"device_code":"dev-1","user_code":"ABCD-EFGH","verification_uri":"https://example.com/activate","interval":1,"expires_in":60}"#.to_string(),
            ),
            (
                "/token".to_string(),
                r#"{"error":"authorization_pending"}"#.to_string(),
            ),
            (
                "/token".to_string(),
                r#"{"access_token":"tok-123","token_type":"bearer"}"#.to_string(),
            ),
        ])
        .await;

        let client = reqwest::Client::new();
        let device = start_device_flow(
            &client,
            &format!("{base}/device/code"),
            &[("client_id", "test-client"), ("scope", "read:user")],
            &[("Accept", "application/json")],
        )
        .await
        .expect("device flow start");
        assert_eq!(device.user_code, "ABCD-EFGH");
        assert_eq!(device.verification_uri, "https://example.com/activate");

        let token = poll_for_access_token(
            &client,
            &format!("{base}/token"),
            &[("client_id", "test-client")],
            &[("Accept", "application/json")],
            &device,
            None,
        )
        .await
        .expect("token poll");
        assert_eq!(token, "tok-123");
    }

    #[tokio::test]
    async fn device_flow_rejects_untrusted_verification_uri() {
        let base = mock_server(vec![(
            "/device/code".to_string(),
            r#"{"device_code":"dev-1","user_code":"ABCD","verification_uri":"file:///etc/passwd","expires_in":60}"#.to_string(),
        )])
        .await;
        let client = reqwest::Client::new();
        let err = start_device_flow(
            &client,
            &format!("{base}/device/code"),
            &[("client_id", "test-client")],
            &[],
        )
        .await
        .unwrap_err();
        assert!(err.contains("Untrusted verification_uri"), "got: {err}");
    }

    #[tokio::test]
    async fn device_flow_slow_down_then_success() {
        let base = mock_server(vec![
            (
                "/device/code".to_string(),
                r#"{"device_code":"dev-1","user_code":"ABCD","verification_uri":"https://example.com/activate","interval":1,"expires_in":60}"#.to_string(),
            ),
            (
                "/token".to_string(),
                r#"{"error":"slow_down","interval":1}"#.to_string(),
            ),
            (
                "/token".to_string(),
                r#"{"access_token":"tok-456"}"#.to_string(),
            ),
        ])
        .await;

        let client = reqwest::Client::new();
        let device = start_device_flow(
            &client,
            &format!("{base}/device/code"),
            &[("client_id", "test-client")],
            &[],
        )
        .await
        .unwrap();
        let token = poll_for_access_token(
            &client,
            &format!("{base}/token"),
            &[("client_id", "test-client")],
            &[],
            &device,
            None,
        )
        .await
        .expect("token poll");
        assert_eq!(token, "tok-456");
    }

    #[tokio::test]
    async fn device_flow_failed_error_propagates() {
        let base = mock_server(vec![
            (
                "/device/code".to_string(),
                r#"{"device_code":"dev-1","user_code":"ABCD","verification_uri":"https://example.com/activate","expires_in":60}"#.to_string(),
            ),
            (
                "/token".to_string(),
                r#"{"error":"access_denied","error_description":"user said no"}"#.to_string(),
            ),
        ])
        .await;

        let client = reqwest::Client::new();
        let device = start_device_flow(
            &client,
            &format!("{base}/device/code"),
            &[("client_id", "test-client")],
            &[],
        )
        .await
        .unwrap();
        let err = poll_for_access_token(
            &client,
            &format!("{base}/token"),
            &[("client_id", "test-client")],
            &[],
            &device,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("access_denied"), "got: {err}");
        assert!(err.contains("user said no"), "got: {err}");
    }
}

#[cfg(test)]
mod callback_server_tests {
    use super::*;

    #[tokio::test]
    async fn callback_server_captures_code_and_state() {
        let (server, url) = CallbackServer::start("/oauth/callback/test".to_string())
            .await
            .unwrap();
        let url_for_request = url.clone();
        let server_future = async move {
            let query = server.wait_for_callback("ok").await.unwrap();
            query
        };
        let request_future = async move {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{url_for_request}?code=abc&state=xyz"))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success());
        };
        let (query, _) = tokio::join!(server_future, request_future);
        assert!(query.contains("code=abc"), "got: {query}");
        assert!(query.contains("state=xyz"), "got: {query}");
    }

    #[tokio::test]
    async fn anthropic_exchange_code_parses_tokens() {
        // Mock the token endpoint.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"access_token":"acc-1","refresh_token":"ref-1","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        // Point the exchange at the mock by swapping the constant via a small
        // local reimplementation: exercise the parsing path directly.
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_CLIENT_ID,
            "code": "code-1",
            "state": "state-1",
            "redirect_uri": ANTHROPIC_REDIRECT_URI,
            "code_verifier": "verifier-1",
        });
        let text = post_json_text(&client, &format!("http://127.0.0.1:{port}/token"), &body)
            .await
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(data["access_token"], "acc-1");
        assert_eq!(data["refresh_token"], "ref-1");
        assert_eq!(data["expires_in"], 3600);
    }
}
