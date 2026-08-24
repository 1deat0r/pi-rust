//! OAuth flow machinery — port of `packages/ai/src/auth/oauth/`
//! (`device-code.ts` + `pkce.ts`).
//!
//! Supplies the RFC 8628 device-code polling loop (with slow_down
//! semantics and abortable sleeps) and PKCE verifier/challenge generation.
//! Provider-specific endpoint definitions (anthropic, github-copilot,
//! openai-codex, radius, ...) layer on top of these primitives.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::auth_flows::{poll_for_access_token, DeviceCodeResponse};
use crate::model_catalog::get_builtin_models;
use serde_json::Value;

pub const DEVICE_CODE_CANCEL_MESSAGE: &str = "Login cancelled";
pub const DEVICE_CODE_TIMEOUT_MESSAGE: &str = "Device flow timed out";
pub const DEVICE_CODE_SLOW_DOWN_TIMEOUT_MESSAGE: &str =
    "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
/// RFC 8628 section 3.2: if the authorization server omits `interval`, the
/// client must use 5 seconds.
pub const DEVICE_CODE_MINIMUM_INTERVAL_MS: u64 = 1000;
pub const DEVICE_CODE_DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5: `slow_down` increases the polling interval by 5s.
pub const DEVICE_CODE_SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// One incomplete poll result (RFC 8628 `pending` / `slow_down` / failure)
/// or a completed value.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<f64> },
    Failed { message: String },
    Complete(T),
}

/// Poll closure: `FnMut()` returning a future that yields the poll result.
pub type DeviceCodePollFn<T> =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = DeviceCodePollResult<T>> + Send>> + Send>;

/// Options for `poll_oauth_device_code_flow` (upstream
/// `OAuthDeviceCodePollOptions`).
pub struct DeviceCodePollOptions<'a, T> {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<u64>,
    pub wait_before_first_poll: bool,
    pub poll: DeviceCodePollFn<T>,
    /// Abort signal (port of `AbortSignal` as a sync flag).
    pub signal: Option<Arc<AtomicBool>>,
    __marker: std::marker::PhantomData<&'a T>,
}

impl<'a, T> DeviceCodePollOptions<'a, T> {
    pub fn new(poll: DeviceCodePollFn<T>) -> Self {
        Self {
            interval_seconds: None,
            expires_in_seconds: None,
            wait_before_first_poll: false,
            poll,
            signal: None,
            __marker: std::marker::PhantomData,
        }
    }
}

/// Sleep that aborts early when the signal fires (upstream `abortableSleep`).
pub async fn abortable_sleep(
    ms: u64,
    signal: Option<&Arc<AtomicBool>>,
    cancel_message: &str,
) -> Result<(), String> {
    if signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false) {
        return Err(cancel_message.to_string());
    }
    let sleep = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false) {
                    return Err(cancel_message.to_string());
                }
            }
        }
    }
}

/// Poll an OAuth device-code flow until completion, failure, or timeout
/// (upstream `pollOAuthDeviceCodeFlow`, RFC 8628).
pub async fn poll_oauth_device_code_flow<T>(
    options: &mut DeviceCodePollOptions<'_, T>,
) -> Result<T, String> {
    let deadline = match options.expires_in_seconds {
        Some(seconds) => Instant::now() + Duration::from_secs(seconds),
        None => Instant::now() + Duration::from_secs(u64::MAX / 2),
    };
    let mut interval_ms = (options
        .interval_seconds
        .unwrap_or(DEVICE_CODE_DEFAULT_POLL_INTERVAL_SECONDS)
        * 1000.0)
        .floor()
        .max(DEVICE_CODE_MINIMUM_INTERVAL_MS as f64) as u64;
    let mut slow_down_responses = 0u32;

    if options.wait_before_first_poll {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            abortable_sleep(
                interval_ms.min(remaining.as_millis() as u64),
                options.signal.as_ref(),
                DEVICE_CODE_CANCEL_MESSAGE,
            )
            .await?;
        }
    }

    loop {
        if options
            .signal
            .as_ref()
            .map(|s| s.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            return Err(DEVICE_CODE_CANCEL_MESSAGE.to_string());
        }
        let result = (options.poll)().await;
        match result {
            DeviceCodePollResult::Complete(value) => return Ok(value),
            DeviceCodePollResult::Failed { message } => return Err(message),
            DeviceCodePollResult::Pending => {}
            DeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                // Server-provided interval wins (GitHub reports the new
                // required minimum); otherwise increment by 5s (RFC 8628 §3.5).
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds.is_finite() && seconds > 0.0 => (seconds * 1000.0)
                        .floor()
                        .max(DEVICE_CODE_MINIMUM_INTERVAL_MS as f64)
                        as u64,
                    _ => interval_ms + DEVICE_CODE_SLOW_DOWN_INTERVAL_INCREMENT_MS,
                };
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        abortable_sleep(
            interval_ms.min(remaining.as_millis() as u64),
            options.signal.as_ref(),
            DEVICE_CODE_CANCEL_MESSAGE,
        )
        .await?;
    }

    if slow_down_responses > 0 {
        Err(DEVICE_CODE_SLOW_DOWN_TIMEOUT_MESSAGE.to_string())
    } else {
        Err(DEVICE_CODE_TIMEOUT_MESSAGE.to_string())
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// Base64url (no padding) encode — upstream `base64urlEncode`.
pub fn base64url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a PKCE verifier + S256 challenge (upstream `generatePKCE`).
pub fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    let rng = ring::rand::SystemRandom::new();
    ring::rand::SecureRandom::fill(&rng, &mut verifier_bytes)
        .expect("system random should fill the verifier");
    let verifier = base64url_encode(&verifier_bytes);
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest as _;
    hasher.update(verifier.as_bytes());
    let challenge = base64url_encode(&hasher.finalize());
    (verifier, challenge)
}

// ---------------------------------------------------------------------------
// GitHub Copilot
// ---------------------------------------------------------------------------

/// GitHub's public OAuth client id used by the upstream Copilot flow.
pub const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// API version sent to the Copilot model catalog endpoint.
pub const GITHUB_COPILOT_API_VERSION: &str = "2026-06-01";

const GITHUB_COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

/// GitHub's three domain-derived OAuth endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCopilotUrls {
    pub device_code_url: String,
    pub access_token_url: String,
    pub copilot_token_url: String,
}

/// Normalize a GitHub Enterprise URL/domain in the same way as upstream's
/// `normalizeDomain`: trim, add an HTTPS scheme when absent, and return only
/// the parsed hostname.
pub fn normalize_github_copilot_domain(input: &str) -> Option<String> {
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
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .filter(|host| !host.is_empty())
}

/// Build the upstream GitHub/GitHub Enterprise OAuth endpoints.
pub fn github_copilot_urls(domain: &str) -> GitHubCopilotUrls {
    GitHubCopilotUrls {
        device_code_url: format!("https://{domain}/login/device/code"),
        access_token_url: format!("https://{domain}/login/oauth/access_token"),
        copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
    }
}

fn github_copilot_base_url_from_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split_once("proxy-ep=")
        .and_then(|(_, rest)| rest.split(';').next())
        .filter(|host| !host.is_empty())?;
    let api_host = proxy_host
        .strip_prefix("proxy.")
        .map(|rest| format!("api.{rest}"))
        .unwrap_or_else(|| proxy_host.to_string());
    Some(format!("https://{api_host}"))
}

/// Resolve the request base URL from a Copilot token, with the same
/// enterprise and individual fallbacks as upstream.
pub fn github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(token) = token {
        if let Some(base_url) = github_copilot_base_url_from_token(token) {
            return base_url;
        }
    }
    enterprise_domain
        .map(|domain| format!("https://copilot-api.{domain}"))
        .unwrap_or_else(|| "https://api.individual.githubcopilot.com".to_string())
}

/// GitHub Copilot OAuth implementation. The optional endpoint override is
/// intentionally public for deterministic local mock fixtures; normal
/// providers use [`GitHubCopilotOAuth::new`] and the real GitHub endpoints.
#[derive(Clone)]
pub struct GitHubCopilotOAuth {
    endpoint_override: Option<String>,
}

impl GitHubCopilotOAuth {
    /// Construct the production Copilot OAuth provider.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            endpoint_override: None,
        })
    }

    /// Construct a provider whose OAuth and model endpoints share a local
    /// base URL. This is used by integration tests and never changes the
    /// production provider wiring.
    pub fn with_base_url(base_url: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            endpoint_override: Some(base_url.into().trim_end_matches('/').to_string()),
        })
    }

    fn urls(&self, domain: &str) -> GitHubCopilotUrls {
        let Some(base_url) = self.endpoint_override.as_deref() else {
            return github_copilot_urls(domain);
        };
        GitHubCopilotUrls {
            device_code_url: format!("{base_url}/login/device/code"),
            access_token_url: format!("{base_url}/login/oauth/access_token"),
            copilot_token_url: format!("{base_url}/copilot_internal/v2/token"),
        }
    }

    fn base_url(&self, token: Option<&str>, enterprise_domain: Option<&str>) -> String {
        self.endpoint_override
            .clone()
            .unwrap_or_else(|| github_copilot_base_url(token, enterprise_domain))
    }

    fn enterprise_domain(credential: &OAuthCredential) -> Option<String> {
        credential
            .extra
            .get("enterpriseUrl")
            .and_then(Value::as_str)
            .and_then(normalize_github_copilot_domain)
    }

    async fn request_json(request: reqwest::RequestBuilder) -> Result<Value, String> {
        let response = request
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| format!("read response: {error}"))?;
        if !status.is_success() {
            return Err(format!("{status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|error| format!("invalid JSON response: {error}"))
    }

    async fn exchange_access_token(
        &self,
        client: &reqwest::Client,
        refresh_token: &str,
        enterprise_domain: Option<&str>,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        if signal.load(Ordering::SeqCst) {
            return Err("Aborted".to_string());
        }
        let domain = enterprise_domain.unwrap_or("github.com");
        let urls = self.urls(domain);
        let mut request = client
            .get(urls.copilot_token_url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {refresh_token}"));
        for (name, value) in GITHUB_COPILOT_HEADERS {
            request = request.header(name, value);
        }
        let raw = Self::request_json(request).await?;
        if !raw.is_object() {
            return Err("Invalid Copilot token response".to_string());
        }
        let token = raw.get("token").and_then(Value::as_str);
        let expires_at = raw
            .get("expires_at")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite());
        let (Some(token), Some(expires_at)) = (token, expires_at) else {
            return Err("Invalid Copilot token response fields".to_string());
        };
        let expires = (expires_at * 1000.0 - 5.0 * 60.0 * 1000.0).max(0.0) as u64;
        let mut extra = BTreeMap::new();
        if let Some(domain) = enterprise_domain {
            extra.insert(
                "enterpriseUrl".to_string(),
                Value::String(domain.to_string()),
            );
        }
        Ok(OAuthCredential {
            refresh: refresh_token.to_string(),
            access: token.to_string(),
            expires,
            extra,
        })
    }

    fn parse_model_catalog(
        raw: &Value,
        allow_policy_fallback: bool,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let data = raw
            .as_object()
            .and_then(|object| object.get("data"))
            .and_then(Value::as_array)
            .ok_or_else(|| "Invalid Copilot models response".to_string())?;

        let mut account_models = Vec::new();
        for item in data {
            let Some(item) = item.as_object() else {
                continue;
            };
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let tool_calls = item
                .get("capabilities")
                .and_then(Value::as_object)
                .and_then(|capabilities| capabilities.get("supports"))
                .and_then(Value::as_object)
                .and_then(|supports| supports.get("tool_calls"))
                .and_then(Value::as_bool);
            if tool_calls == Some(false) {
                continue;
            }
            let picker_enabled = item
                .get("model_picker_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let policy_state = item
                .get("policy")
                .and_then(Value::as_object)
                .and_then(|policy| policy.get("state"))
                .and_then(Value::as_str)
                .map(str::to_string);
            account_models.push((id.to_string(), picker_enabled, policy_state));
        }

        let picker_model_ids: Vec<String> = account_models
            .iter()
            .filter(|(_, picker_enabled, policy_state)| {
                *picker_enabled && policy_state.as_deref() != Some("disabled")
            })
            .map(|(id, _, _)| id.clone())
            .collect();
        let use_policy_fallback = allow_policy_fallback && picker_model_ids.is_empty();
        let available_model_ids = if !picker_model_ids.is_empty() || !allow_policy_fallback {
            picker_model_ids
        } else {
            account_models
                .iter()
                .filter(|(_, _, policy_state)| policy_state.as_deref() == Some("enabled"))
                .map(|(id, _, _)| id.clone())
                .collect()
        };
        let policy_model_ids = account_models
            .iter()
            .filter(|(id, picker_enabled, policy_state)| {
                policy_state.as_deref() == Some("unconfigured")
                    && get_builtin_models("github-copilot")
                        .iter()
                        .any(|model| model.id == *id)
                    && (*picker_enabled || use_policy_fallback)
            })
            .map(|(id, _, _)| id.clone())
            .collect();
        Ok((available_model_ids, policy_model_ids))
    }

    fn retry_after_ms(response: &reqwest::Response, retry: u32) -> Option<u64> {
        let Some(header) = response.headers().get("retry-after") else {
            return Some(500u64.saturating_mul(2u64.saturating_pow(retry)));
        };
        let Ok(value) = header.to_str().map(str::parse::<f64>) else {
            return None;
        };
        let Ok(value) = value else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        Some((value.max(0.0) * 1000.0) as u64)
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        token: &str,
        enterprise_domain: Option<&str>,
        signal: &AtomicBool,
        max_retries: u32,
        max_elapsed_ms: u64,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let base_url = self.base_url(Some(token), enterprise_domain);
        let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
        let url = format!("{base_url}/models");
        let started = Instant::now();
        for retry in 0..=max_retries {
            if signal.load(Ordering::SeqCst) {
                return Err("Aborted".to_string());
            }
            let mut request = client
                .get(&url)
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .header("X-GitHub-Api-Version", GITHUB_COPILOT_API_VERSION);
            for (name, value) in GITHUB_COPILOT_HEADERS {
                request = request.header(name, value);
            }
            let response = request
                .send()
                .await
                .map_err(|error| format!("request failed: {error}"))?;
            if response.status().as_u16() == 429 && retry < max_retries {
                let Some(delay_ms) = Self::retry_after_ms(&response, retry) else {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("read response: {error}"))?;
                    return Err(format!("{status}: {text}"));
                };
                let Some(deadline) = started.checked_add(Duration::from_millis(max_elapsed_ms))
                else {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("read response: {error}"))?;
                    return Err(format!("{status}: {text}"));
                };
                if max_elapsed_ms == 0
                    || Instant::now()
                        .checked_add(Duration::from_millis(delay_ms))
                        .is_none_or(|wake| wake >= deadline)
                {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("read response: {error}"))?;
                    return Err(format!("{status}: {text}"));
                }
                drop(response);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|error| format!("read response: {error}"))?;
            if !status.is_success() {
                return Err(format!("{status}: {text}"));
            }
            let raw: Value = serde_json::from_str(&text)
                .map_err(|error| format!("invalid JSON response: {error}"))?;
            return Self::parse_model_catalog(&raw, allow_policy_fallback);
        }
        unreachable!("Copilot model retry loop always returns");
    }

    async fn start_device_flow(
        &self,
        client: &reqwest::Client,
        domain: &str,
    ) -> Result<DeviceCodeResponse, String> {
        let urls = self.urls(domain);
        let raw = Self::request_json(
            client
                .post(urls.device_code_url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", "GitHubCopilotChat/0.35.0")
                .form(&[
                    ("client_id", GITHUB_COPILOT_CLIENT_ID),
                    ("scope", "read:user"),
                ]),
        )
        .await?;
        let Some(object) = raw.as_object() else {
            return Err("Invalid device code response".to_string());
        };
        let device_code = object.get("device_code").and_then(Value::as_str);
        let user_code = object.get("user_code").and_then(Value::as_str);
        let verification_uri = object.get("verification_uri").and_then(Value::as_str);
        let interval = object.get("interval");
        let expires_in = object
            .get("expires_in")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite());
        if device_code.is_none()
            || user_code.is_none()
            || verification_uri.is_none()
            || interval.is_some_and(|value| !value.is_number())
            || expires_in.is_none()
        {
            return Err("Invalid device code response fields".to_string());
        }
        let verification_uri = verification_uri.unwrap();
        let parsed = url::Url::parse(verification_uri)
            .map_err(|_| "Untrusted verification_uri in device code response".to_string())?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err("Untrusted verification_uri in device code response".to_string());
        }
        Ok(DeviceCodeResponse {
            device_code: device_code.unwrap().to_string(),
            user_code: user_code.unwrap().to_string(),
            verification_uri: parsed.to_string(),
            interval: interval.and_then(Value::as_f64),
            expires_in: expires_in.unwrap().max(0.0) as u64,
        })
    }

    async fn enable_model(
        &self,
        client: &reqwest::Client,
        token: &str,
        enterprise_domain: Option<&str>,
        model_id: &str,
        signal: &AtomicBool,
    ) -> Result<bool, String> {
        if signal.load(Ordering::SeqCst) {
            return Err("Aborted".to_string());
        }
        let url = format!(
            "{}/models/{model_id}/policy",
            self.base_url(Some(token), enterprise_domain)
        );
        let started = Instant::now();
        for retry in 0..=2 {
            let mut request = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .header("openai-intent", "chat-policy")
                .header("x-interaction-type", "chat-policy")
                .json(&serde_json::json!({"state": "enabled"}));
            for (name, value) in GITHUB_COPILOT_HEADERS {
                request = request.header(name, value);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) => return Ok(false),
            };
            if response.status().as_u16() == 429 && retry < 2 {
                let Some(delay_ms) = Self::retry_after_ms(&response, retry) else {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("read response: {error}"))?;
                    return Err(format!("{status}: {text}"));
                };
                let deadline = started + Duration::from_millis(5_000);
                if Instant::now()
                    .checked_add(Duration::from_millis(delay_ms))
                    .is_none_or(|wake| wake >= deadline)
                {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|error| format!("read response: {error}"))?;
                    return Err(format!("{status}: {text}"));
                }
                drop(response);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }
            if response.status().as_u16() == 429 {
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|error| format!("read response: {error}"))?;
                return Err(format!("{status}: {text}"));
            }
            return Ok(response.status().is_success());
        }
        unreachable!("Copilot policy retry loop always returns");
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
        let enterprise_domain = normalize_github_copilot_domain(&input);
        if !trimmed.is_empty() && enterprise_domain.is_none() {
            return Err("Invalid GitHub Enterprise URL/domain".to_string());
        }
        let domain = enterprise_domain
            .clone()
            .unwrap_or_else(|| "github.com".to_string());
        let client = reqwest::Client::new();
        let device = self.start_device_flow(&client, &domain).await?;
        interaction.notify(&AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
        });
        let urls = self.urls(&domain);
        let github_access_token = poll_for_access_token(
            &client,
            &urls.access_token_url,
            &[("client_id", GITHUB_COPILOT_CLIENT_ID)],
            &[
                ("Accept", "application/json"),
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("User-Agent", "GitHubCopilotChat/0.35.0"),
            ],
            &device,
            None,
        )
        .await?;
        let mut credentials = self
            .exchange_access_token(
                &client,
                &github_access_token,
                enterprise_domain.as_deref(),
                &AtomicBool::new(false),
            )
            .await?;
        let (available_model_ids, policy_model_ids) = self
            .fetch_models(
                &client,
                &credentials.access,
                enterprise_domain.as_deref(),
                &AtomicBool::new(false),
                2,
                5_000,
            )
            .await?;
        let signal = AtomicBool::new(false);
        let mut enabled_model_ids = Vec::new();
        if !policy_model_ids.is_empty() {
            interaction.notify(&AuthEvent::Progress {
                message: "Enabling models...".to_string(),
            });
            for model_id in &policy_model_ids {
                match self
                    .enable_model(
                        &client,
                        &credentials.access,
                        enterprise_domain.as_deref(),
                        model_id,
                        &signal,
                    )
                    .await
                {
                    Ok(true) => enabled_model_ids.push(model_id.clone()),
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        }
        let mut all_model_ids = available_model_ids;
        all_model_ids.extend(enabled_model_ids);
        credentials.extra.insert(
            "availableModelIds".to_string(),
            Value::Array(all_model_ids.into_iter().map(Value::String).collect()),
        );
        Ok(credentials)
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, String> {
        let enterprise_domain = Self::enterprise_domain(credential);
        let client = reqwest::Client::new();
        let mut credentials = self
            .exchange_access_token(
                &client,
                &credential.refresh,
                enterprise_domain.as_deref(),
                signal,
            )
            .await?;
        let (available_model_ids, _) = self
            .fetch_models(
                &client,
                &credentials.access,
                enterprise_domain.as_deref(),
                signal,
                0,
                0,
            )
            .await?;
        credentials.extra.insert(
            "availableModelIds".to_string(),
            Value::Array(available_model_ids.into_iter().map(Value::String).collect()),
        );
        Ok(credentials)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Option<ModelAuth> {
        let enterprise_domain = Self::enterprise_domain(credential);
        Some(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url: Some(self.base_url(Some(&credential.access), enterprise_domain.as_deref())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_shapes() {
        let (verifier, challenge) = generate_pkce();
        assert_eq!(verifier.len(), 43, "verifier {verifier}");
        assert_eq!(challenge.len(), 43, "challenge {challenge}");
        assert_ne!(verifier, challenge);
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn completes_on_first_poll() {
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::Complete(42u32) })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(30);
        let value = poll_oauth_device_code_flow(&mut options).await.unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn handles_pending_slow_down_then_complete() {
        let states: Vec<DeviceCodePollResult<&'static str>> = vec![
            DeviceCodePollResult::Pending,
            DeviceCodePollResult::SlowDown {
                interval_seconds: None,
            },
            DeviceCodePollResult::SlowDown {
                interval_seconds: Some(1.0),
            },
            DeviceCodePollResult::Complete("done"),
        ];
        let mut index = 0usize;
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            let state = states
                .get(index)
                .cloned()
                .unwrap_or(DeviceCodePollResult::Complete("done"));
            index += 1;
            Box::pin(async move { state })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(30);
        let start = Instant::now();
        let value = poll_oauth_device_code_flow(&mut options).await.unwrap();
        assert_eq!(value, "done");
        // Pending -> 1s sleep; SlowDown(None) -> interval += 5s but the NEXT
        // sleep is min(6s, remaining) ... server SlowDown(1.0) resets to 1s,
        // then complete. Expect at least ~2s of sleeps.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1900),
            "elapsed {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(12), "elapsed {elapsed:?}");
    }

    #[tokio::test]
    async fn aborts_with_signal() {
        let signal = Arc::new(AtomicBool::new(false));
        let sig = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sig.store(true, Ordering::SeqCst);
        });
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::<()>::Pending })
        }));
        options.interval_seconds = Some(30.0);
        options.expires_in_seconds = Some(60);
        options.signal = Some(signal);
        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();
        assert_eq!(err, DEVICE_CODE_CANCEL_MESSAGE);
    }

    #[tokio::test]
    async fn timeout_when_all_pending() {
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::<()>::Pending })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(2);
        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();
        assert_eq!(err, DEVICE_CODE_TIMEOUT_MESSAGE);
    }

    #[test]
    fn base64url_no_padding() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"f\xfb"), "Zvs");
        assert_eq!(base64url_encode(b"f\xfb\xff"), "Zvv_");
    }
}
