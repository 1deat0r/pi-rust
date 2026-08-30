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
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthSelectOption, ModelAuth, OAuthAuth,
    OAuthCredential, OAuthFailureKind,
};
use crate::auth_flows::{poll_for_access_token, DeviceCodeResponse};
use crate::error::PiAiError;
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
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(10);

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
) -> Result<(), PiAiError> {
    if signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false) {
        return Err(PiAiError::other(cancel_message));
    }
    let sleep = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(sleep);
    if let Some(signal) = signal {
        let abort = wait_for_atomic_abort(signal.clone());
        tokio::pin!(abort);
        tokio::select! {
            _ = &mut sleep => Ok(()),
            _ = &mut abort => Err(PiAiError::other(cancel_message)),
        }
    } else {
        sleep.await;
        Ok(())
    }
}

async fn wait_for_atomic_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_atomic_abort_ref(signal: &AtomicBool) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn abortable_sleep_ref(
    ms: u64,
    signal: &AtomicBool,
    cancel_message: &str,
) -> Result<(), PiAiError> {
    if signal.load(Ordering::SeqCst) {
        return Err(PiAiError::other(cancel_message));
    }
    let sleep = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(sleep);
    let abort = wait_for_atomic_abort_ref(signal);
    tokio::pin!(abort);
    tokio::select! {
        _ = &mut sleep => Ok(()),
        _ = &mut abort => Err(PiAiError::other(cancel_message)),
    }
}

fn safe_http_error(operation: &str, phase: &str, error: &reqwest::Error) -> PiAiError {
    PiAiError::http(safe_http_error_string(operation, phase, error))
}

fn safe_http_error_string(operation: &str, phase: &str, error: &reqwest::Error) -> String {
    // reqwest may include the full endpoint in its Display output. Auth
    // endpoints can be caller-configured, so keep URL credentials and query
    // material out of user-visible OAuth errors.
    if error.is_timeout() {
        format!("{operation} {phase} timed out")
    } else {
        format!("{operation} {phase} failed")
    }
}

async fn request_with_optional_abort(
    request: reqwest::RequestBuilder,
    signal: Option<&AtomicBool>,
    operation: &str,
) -> Result<reqwest::Response, PiAiError> {
    if signal.is_some_and(|signal| signal.load(Ordering::SeqCst)) {
        return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
    }
    let request = request.send();
    tokio::pin!(request);
    let timeout = tokio::time::sleep(OAUTH_HTTP_TIMEOUT);
    tokio::pin!(timeout);
    match signal {
        Some(signal) => {
            let abort = wait_for_atomic_abort_ref(signal);
            tokio::pin!(abort);
            tokio::select! {
                response = &mut request => response.map_err(|error| safe_http_error(operation, "request", &error)),
                _ = &mut abort => Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE)),
                _ = &mut timeout => Err(PiAiError::timeout(format!("{operation} request timed out"))),
            }
        }
        None => tokio::select! {
            response = &mut request => response.map_err(|error| safe_http_error(operation, "request", &error)),
            _ = &mut timeout => Err(PiAiError::timeout(format!("{operation} request timed out"))),
        },
    }
}

async fn response_text_with_optional_abort(
    response: reqwest::Response,
    signal: Option<&AtomicBool>,
    operation: &str,
) -> Result<String, PiAiError> {
    let response = response.text();
    tokio::pin!(response);
    let timeout = tokio::time::sleep(OAUTH_HTTP_TIMEOUT);
    tokio::pin!(timeout);
    match signal {
        Some(signal) => {
            let abort = wait_for_atomic_abort_ref(signal);
            tokio::pin!(abort);
            tokio::select! {
                text = &mut response => text.map_err(|error| safe_http_error(operation, "response read", &error)),
                _ = &mut abort => Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE)),
                _ = &mut timeout => Err(PiAiError::timeout(format!("{operation} response read timed out"))),
            }
        }
        None => tokio::select! {
            text = &mut response => text.map_err(|error| safe_http_error(operation, "response read", &error)),
            _ = &mut timeout => Err(PiAiError::timeout(format!("{operation} response read timed out"))),
        },
    }
}

/// Read one browser callback request without losing cancellation responsiveness
/// after a client has connected but stopped sending bytes.
async fn read_callback_request(
    socket: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
    cancel: &Arc<AtomicBool>,
) -> Result<usize, PiAiError> {
    use tokio::io::AsyncReadExt;

    let read_headers = async {
        let mut total = 0usize;
        loop {
            if total == buffer.len() {
                return Ok(total);
            }
            let read = socket.read(&mut buffer[total..]).await;
            let read = read.map_err(|error| format!("OAuth callback read failed: {error}"))?;
            if read == 0 {
                return Ok(total);
            }
            total += read;
            if buffer[..total]
                .windows(4)
                .any(|window| window == b"\r\n\r\n")
            {
                return Ok(total);
            }
        }
    };
    tokio::pin!(read_headers);
    let timeout = tokio::time::sleep(OAUTH_CALLBACK_READ_TIMEOUT);
    tokio::pin!(timeout);
    let abort = wait_for_atomic_abort(cancel.clone());
    tokio::pin!(abort);
    tokio::select! {
        result = &mut read_headers => result,
        _ = &mut abort => Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE)),
        _ = &mut timeout => Err(PiAiError::timeout("OAuth callback read timed out")),
    }
}

fn redact_secrets(detail: &str, secrets: &[&str]) -> String {
    let mut detail = detail.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            detail = detail.replace(secret, "<redacted>");
        }
    }
    if detail.chars().count() > 512 {
        let end = detail
            .char_indices()
            .nth(512)
            .map(|(index, _)| index)
            .unwrap_or(detail.len());
        detail.truncate(end);
        detail.push('…');
    }
    detail
}

fn redacted_json_error_detail(body: &str, secrets: &[&str]) -> Option<String> {
    let json = serde_json::from_str::<Value>(body).ok()?;
    let object = json.as_object()?;
    let detail = if object.len() == 1 && object.get("error").and_then(Value::as_str).is_some() {
        serde_json::to_string(object).ok()?
    } else {
        ["error_description", "message", "error", "code"]
            .into_iter()
            .find_map(|field| object.get(field).and_then(Value::as_str))
            .or_else(|| {
                object
                    .get("error")
                    .and_then(Value::as_object)
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
            })?
            .to_string()
    };
    Some(redact_secrets(&detail, secrets))
}

fn http_error(status: reqwest::StatusCode, body: &str, secrets: &[&str]) -> String {
    match redacted_json_error_detail(body, secrets) {
        Some(detail) => format!("{status}: {detail}"),
        None => status.to_string(),
    }
}

fn openai_codex_status_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
    secrets: &[&str],
) -> String {
    let body = redact_secrets(body, secrets);
    if body.is_empty() {
        format!(
            "OpenAI Codex {operation} failed with status {}",
            status.as_u16()
        )
    } else {
        format!(
            "OpenAI Codex {operation} failed with status {}: {body}",
            status.as_u16()
        )
    }
}

fn openai_codex_error_detail(body: &str, secrets: &[&str]) -> Option<String> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    let object = value.as_object()?;
    let detail = ["error_description", "message", "error", "code"]
        .into_iter()
        .find_map(|field| object.get(field).and_then(Value::as_str))
        .or_else(|| {
            object
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| {
                    error
                        .get("message")
                        .or_else(|| error.get("error_description"))
                        .or_else(|| error.get("code"))
                })
                .and_then(Value::as_str)
        })?;
    Some(redact_secrets(detail, secrets))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days_in_month {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn parse_http_date_delay_ms(value: &str) -> Option<u64> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    parse_http_date_delay_ms_at(value, now_ms)
}

fn parse_http_date_delay_ms_at(value: &str, now_ms: u64) -> Option<u64> {
    let (_, date) = value.trim().split_once(',')?;
    let fields = date.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 || !fields[4].eq_ignore_ascii_case("GMT") {
        return None;
    }
    let day = fields[0].parse::<u32>().ok()?;
    let month = match fields[1].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year = fields[2].parse::<i64>().ok()?;
    let time = fields[3];
    let time_fields = time.split(':').collect::<Vec<_>>();
    if time_fields.len() != 3 {
        return None;
    }
    let hour = time_fields[0].parse::<u64>().ok()?;
    let minute = time_fields[1].parse::<u64>().ok()?;
    let second = time_fields[2].parse::<u64>().ok()?;
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)? as i128;
    let target_ms = days * 86_400_000
        + i128::from(hour) * 3_600_000
        + i128::from(minute) * 60_000
        + i128::from(second) * 1_000;
    u64::try_from((target_ms - i128::from(now_ms)).max(0)).ok()
}

fn interval_to_ms(seconds: Option<f64>, fallback: u64) -> u64 {
    let seconds = match seconds {
        Some(seconds) if seconds.is_finite() => seconds,
        Some(_) => return fallback,
        None => return fallback,
    };
    let millis = (seconds * 1000.0).floor();
    if !millis.is_finite() {
        return u64::MAX;
    }
    millis
        .max(DEVICE_CODE_MINIMUM_INTERVAL_MS as f64)
        .min(u64::MAX as f64) as u64
}

async fn poll_once<T>(
    poll: &mut DeviceCodePollFn<T>,
    signal: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
) -> Result<Option<DeviceCodePollResult<T>>, PiAiError> {
    match (signal, deadline) {
        (Some(signal), Some(deadline)) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let poll_future = poll();
            tokio::pin!(poll_future);
            let abort = wait_for_atomic_abort(signal);
            tokio::pin!(abort);
            let timeout = tokio::time::sleep(remaining);
            tokio::pin!(timeout);
            tokio::select! {
                result = &mut poll_future => Ok(Some(result)),
                _ = &mut abort => Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE)),
                _ = &mut timeout => Ok(None),
            }
        }
        (Some(signal), None) => {
            let poll_future = poll();
            tokio::pin!(poll_future);
            let abort = wait_for_atomic_abort(signal);
            tokio::pin!(abort);
            tokio::select! {
                result = &mut poll_future => Ok(Some(result)),
                _ = &mut abort => Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE)),
            }
        }
        (None, Some(deadline)) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let poll_future = poll();
            tokio::pin!(poll_future);
            let timeout = tokio::time::sleep(remaining);
            tokio::pin!(timeout);
            tokio::select! {
                result = &mut poll_future => Ok(Some(result)),
                _ = &mut timeout => Ok(None),
            }
        }
        (None, None) => Ok(Some(poll().await)),
    }
}

/// Poll an OAuth device-code flow until completion, failure, or timeout
/// (upstream `pollOAuthDeviceCodeFlow`, RFC 8628).
pub async fn poll_oauth_device_code_flow<T>(
    options: &mut DeviceCodePollOptions<'_, T>,
) -> Result<T, PiAiError> {
    let deadline = options
        .expires_in_seconds
        .and_then(|seconds| Instant::now().checked_add(Duration::from_secs(seconds)));
    let mut interval_ms = interval_to_ms(
        options.interval_seconds,
        (DEVICE_CODE_DEFAULT_POLL_INTERVAL_SECONDS * 1000.0) as u64,
    );
    let mut slow_down_responses = 0u32;

    if options.wait_before_first_poll {
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                abortable_sleep(
                    interval_ms.min(remaining.as_millis() as u64),
                    options.signal.as_ref(),
                    DEVICE_CODE_CANCEL_MESSAGE,
                )
                .await?;
            }
        } else {
            abortable_sleep(
                interval_ms,
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
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
        }
        // An expired authorization window must not issue one final poll. This
        // is observable for zero/very short lifetimes and matches the
        // upstream loop's `Date.now() < deadline` guard.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let result = match poll_once(&mut options.poll, options.signal.clone(), deadline).await? {
            Some(result) => result,
            None => break,
        };
        match result {
            DeviceCodePollResult::Complete(value) => return Ok(value),
            DeviceCodePollResult::Failed { message } => return Err(PiAiError::other(message)),
            DeviceCodePollResult::Pending => {}
            DeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                // Server-provided interval wins (GitHub reports the new
                // required minimum); otherwise increment by 5s (RFC 8628 §3.5).
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                        interval_to_ms(Some(seconds), interval_ms)
                    }
                    _ => interval_ms.saturating_add(DEVICE_CODE_SLOW_DOWN_INTERVAL_INCREMENT_MS),
                };
            }
        }
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            break;
        }
        abortable_sleep(
            remaining
                .map(|remaining| interval_ms.min(remaining.as_millis() as u64))
                .unwrap_or(interval_ms),
            options.signal.as_ref(),
            DEVICE_CODE_CANCEL_MESSAGE,
        )
        .await?;
    }

    if slow_down_responses > 0 {
        Err(PiAiError::timeout(DEVICE_CODE_SLOW_DOWN_TIMEOUT_MESSAGE))
    } else {
        Err(PiAiError::timeout(DEVICE_CODE_TIMEOUT_MESSAGE))
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
    // Invariant: the OS random source failing is unrecoverable for OAuth.
    #[allow(clippy::expect_used)]
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

    async fn request_json(
        request: reqwest::RequestBuilder,
        signal: Option<&AtomicBool>,
        operation: &str,
        secrets: &[&str],
    ) -> Result<Value, PiAiError> {
        let response = request_with_optional_abort(request, signal, operation).await?;
        let status = response.status();
        let text = response_text_with_optional_abort(response, signal, operation).await?;
        if !status.is_success() {
            return Err(PiAiError::http(http_error(status, &text, secrets)));
        }
        serde_json::from_str(&text)
            .map_err(|error| PiAiError::invalid_response(format!("invalid JSON response: {error}")))
    }

    async fn exchange_access_token(
        &self,
        client: &reqwest::Client,
        refresh_token: &str,
        enterprise_domain: Option<&str>,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, PiAiError> {
        if signal.load(Ordering::SeqCst) {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
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
        let raw =
            Self::request_json(request, Some(signal), "Copilot token", &[refresh_token]).await?;
        if !raw.is_object() {
            return Err(PiAiError::invalid_response(
                "Invalid Copilot token response",
            ));
        }
        let token = raw.get("token").and_then(Value::as_str);
        let expires_at = raw
            .get("expires_at")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite());
        let (Some(token), Some(expires_at)) = (token.filter(|token| !token.is_empty()), expires_at)
        else {
            return Err(PiAiError::invalid_response(
                "Invalid Copilot token response fields",
            ));
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
    ) -> Result<(Vec<String>, Vec<String>), PiAiError> {
        let data = raw
            .as_object()
            .and_then(|object| object.get("data"))
            .and_then(Value::as_array)
            .ok_or_else(|| PiAiError::invalid_response("Invalid Copilot models response"))?;

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

    fn retry_after_ms(response: &reqwest::Response) -> Option<u64> {
        // Copilot only asks us to repeat a throttled catalog/policy request
        // when the service supplies an explicit Retry-After.  Blindly
        // inventing a backoff turns a single 429 into a second network call,
        // and masks the provider's actionable response (the upstream flow
        // preserves that response instead).
        let header = response.headers().get("retry-after")?;
        let Ok(value) = header.to_str() else {
            return None;
        };
        if let Ok(seconds) = value.trim().parse::<f64>() {
            if seconds.is_finite() {
                return Some((seconds.max(0.0) * 1000.0) as u64);
            }
        }
        parse_http_date_delay_ms(value)
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        token: &str,
        enterprise_domain: Option<&str>,
        signal: &AtomicBool,
        max_retries: u32,
        max_elapsed_ms: u64,
    ) -> Result<(Vec<String>, Vec<String>), PiAiError> {
        let base_url = self.base_url(Some(token), enterprise_domain);
        let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
        let url = format!("{base_url}/models");
        let started = Instant::now();
        for retry in 0..=max_retries {
            if signal.load(Ordering::SeqCst) {
                return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
            }
            let mut request = client
                .get(&url)
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .header("X-GitHub-Api-Version", GITHUB_COPILOT_API_VERSION);
            for (name, value) in GITHUB_COPILOT_HEADERS {
                request = request.header(name, value);
            }
            let response =
                request_with_optional_abort(request, Some(signal), "Copilot models").await?;
            if response.status().as_u16() == 429 && retry < max_retries {
                let Some(delay_ms) = Self::retry_after_ms(&response) else {
                    let status = response.status();
                    let text =
                        response_text_with_optional_abort(response, Some(signal), "Copilot models")
                            .await?;
                    return Err(PiAiError::http(http_error(status, &text, &[token])));
                };
                let Some(deadline) = started.checked_add(Duration::from_millis(max_elapsed_ms))
                else {
                    let status = response.status();
                    let text =
                        response_text_with_optional_abort(response, Some(signal), "Copilot models")
                            .await?;
                    return Err(PiAiError::http(http_error(status, &text, &[token])));
                };
                if max_elapsed_ms == 0
                    || Instant::now()
                        .checked_add(Duration::from_millis(delay_ms))
                        .is_none_or(|wake| wake >= deadline)
                {
                    let status = response.status();
                    let text =
                        response_text_with_optional_abort(response, Some(signal), "Copilot models")
                            .await?;
                    return Err(PiAiError::http(http_error(status, &text, &[token])));
                }
                drop(response);
                abortable_sleep_ref(delay_ms, signal, DEVICE_CODE_CANCEL_MESSAGE).await?;
                continue;
            }
            let status = response.status();
            let text =
                response_text_with_optional_abort(response, Some(signal), "Copilot models").await?;
            if !status.is_success() {
                return Err(PiAiError::http(http_error(status, &text, &[token])));
            }
            let raw: Value = serde_json::from_str(&text).map_err(|error| {
                PiAiError::invalid_response(format!("invalid JSON response: {error}"))
            })?;
            return Self::parse_model_catalog(&raw, allow_policy_fallback);
        }
        unreachable!("Copilot model retry loop always returns");
    }

    #[allow(clippy::expect_used)] // field presence validated in the guard above
    async fn start_device_flow(
        &self,
        client: &reqwest::Client,
        domain: &str,
        signal: &AtomicBool,
    ) -> Result<DeviceCodeResponse, PiAiError> {
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
            Some(signal),
            "Copilot device code",
            &[],
        )
        .await?;
        let Some(object) = raw.as_object() else {
            return Err(PiAiError::invalid_response("Invalid device code response"));
        };
        let device_code = object
            .get("device_code")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let user_code = object
            .get("user_code")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let verification_uri = object
            .get("verification_uri")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let interval = object
            .get("interval")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite());
        let expires_in = object
            .get("expires_in")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0);
        if device_code.is_none()
            || user_code.is_none()
            || verification_uri.is_none()
            || object
                .get("interval")
                .is_some_and(|value| interval.is_none() || !value.is_number())
            || expires_in.is_none()
        {
            return Err(PiAiError::invalid_response(
                "Invalid device code response fields",
            ));
        }
        let Some(verification_uri) = verification_uri else {
            return Err(PiAiError::invalid_response(
                "Invalid device code response fields",
            ));
        };
        let parsed = url::Url::parse(verification_uri).map_err(|_| {
            PiAiError::invalid_response("Untrusted verification_uri in device code response")
        })?;
        if (parsed.scheme() != "http" && parsed.scheme() != "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(PiAiError::invalid_response(
                "Untrusted verification_uri in device code response",
            ));
        }
        let device_code = device_code
            .ok_or_else(|| PiAiError::invalid_response("Invalid device code response fields"))?;
        let user_code = user_code
            .ok_or_else(|| PiAiError::invalid_response("Invalid device code response fields"))?;
        Ok(DeviceCodeResponse {
            device_code: device_code.to_string(),
            user_code: user_code.to_string(),
            verification_uri: parsed.to_string(),
            interval,
            expires_in: expires_in
                .expect("expires_in presence checked above")
                .floor() as u64,
        })
    }

    async fn enable_model(
        &self,
        client: &reqwest::Client,
        token: &str,
        enterprise_domain: Option<&str>,
        model_id: &str,
        signal: &AtomicBool,
    ) -> Result<bool, PiAiError> {
        if signal.load(Ordering::SeqCst) {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
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
            let response =
                match request_with_optional_abort(request, Some(signal), "Copilot policy").await {
                    Ok(response) => response,
                    Err(error) => {
                        if signal.load(Ordering::SeqCst) {
                            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
                        }
                        return Err(error);
                    }
                };
            if response.status().as_u16() == 429 && retry < 2 {
                let Some(delay_ms) = Self::retry_after_ms(&response) else {
                    let status = response.status();
                    let text =
                        response_text_with_optional_abort(response, Some(signal), "Copilot policy")
                            .await?;
                    return Err(PiAiError::http(http_error(status, &text, &[token])));
                };
                let Some(deadline) = started.checked_add(Duration::from_millis(5_000)) else {
                    return Ok(false);
                };
                if Instant::now()
                    .checked_add(Duration::from_millis(delay_ms))
                    .is_none_or(|wake| wake >= deadline)
                {
                    let status = response.status();
                    let text =
                        response_text_with_optional_abort(response, Some(signal), "Copilot policy")
                            .await?;
                    return Err(PiAiError::http(http_error(status, &text, &[token])));
                }
                drop(response);
                abortable_sleep_ref(delay_ms, signal, DEVICE_CODE_CANCEL_MESSAGE).await?;
                continue;
            }
            if response.status().as_u16() == 429 {
                let status = response.status();
                let text =
                    response_text_with_optional_abort(response, Some(signal), "Copilot policy")
                        .await?;
                return Err(PiAiError::http(http_error(status, &text, &[token])));
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

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, PiAiError> {
        let input = interaction.prompt(&crate::auth::AuthPrompt::Text {
            message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
            placeholder: Some("company.ghe.com".to_string()),
        })?;
        let trimmed = input.trim();
        let enterprise_domain = normalize_github_copilot_domain(&input);
        if !trimmed.is_empty() && enterprise_domain.is_none() {
            return Err(PiAiError::invalid_response(
                "Invalid GitHub Enterprise URL/domain",
            ));
        }
        let domain = enterprise_domain
            .clone()
            .unwrap_or_else(|| "github.com".to_string());
        let signal = interaction
            .signal()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        if signal.load(Ordering::SeqCst) {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
        }
        let client = reqwest::Client::new();
        let device = self
            .start_device_flow(&client, &domain, signal.as_ref())
            .await?;
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
            Some(&signal),
        )
        .await?;
        let mut credentials = self
            .exchange_access_token(
                &client,
                &github_access_token,
                enterprise_domain.as_deref(),
                signal.as_ref(),
            )
            .await?;
        let (available_model_ids, policy_model_ids) = self
            .fetch_models(
                &client,
                &credentials.access,
                enterprise_domain.as_deref(),
                signal.as_ref(),
                2,
                5_000,
            )
            .await?;
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
                    Err(error) => {
                        if signal.load(Ordering::SeqCst) {
                            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
                        }
                        let _ = error;
                        break;
                    }
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
    ) -> Result<OAuthCredential, PiAiError> {
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

// ---------------------------------------------------------------------------
// OpenAI Codex (ChatGPT subscription) OAuth
// ---------------------------------------------------------------------------

/// Public OAuth client identifier used by the upstream Codex login flow.
pub const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_CODEX_AUTH_BASE_URL: &str = "https://auth.openai.com";
pub const OPENAI_CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const OPENAI_CODEX_CALLBACK_PORT: u16 = 1455;
pub const OPENAI_CODEX_DEVICE_TIMEOUT_SECONDS: u64 = 15 * 60;
pub const OPENAI_CODEX_SCOPE: &str = "openid profile email offline_access";
const OPENAI_CODEX_BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAICodexUrls {
    pub authorize_url: String,
    pub token_url: String,
    pub device_user_code_url: String,
    pub device_token_url: String,
    pub device_verification_uri: String,
    pub device_redirect_uri: String,
}

/// Build the endpoint set from an authority base URL. Production uses
/// `https://auth.openai.com`; the injectable base is intentionally public so
/// tests can exercise the complete flow against loopback HTTP fixtures.
pub fn openai_codex_urls(base_url: &str) -> OpenAICodexUrls {
    let base_url = base_url.trim_end_matches('/');
    OpenAICodexUrls {
        authorize_url: format!("{base_url}/oauth/authorize"),
        token_url: format!("{base_url}/oauth/token"),
        device_user_code_url: format!("{base_url}/api/accounts/deviceauth/usercode"),
        device_token_url: format!("{base_url}/api/accounts/deviceauth/token"),
        device_verification_uri: format!("{base_url}/codex/device"),
        device_redirect_uri: format!("{base_url}/deviceauth/callback"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAICodexAuthorizationInput {
    pub code: Option<String>,
    pub state: Option<String>,
}

fn openai_codex_authorization_query(input: &str) -> Option<Vec<(String, String)>> {
    let value = input.trim();
    if let Ok(url) = url::Url::parse(value) {
        return Some(
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
        );
    }
    let query = value.trim_start_matches(['?', '&']);
    let looks_like_query = query != value
        || query.split('&').any(|part| {
            part.starts_with("code=") || part.starts_with("error=") || part.starts_with("state=")
        });
    looks_like_query.then(|| {
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    })
}

/// Accept the forms supported by the upstream CLI: a full callback URL,
/// `code#state`, query-string parameters, or a raw authorization code.
pub fn parse_openai_codex_authorization_input(input: &str) -> OpenAICodexAuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return OpenAICodexAuthorizationInput {
            code: None,
            state: None,
        };
    }
    if let Some(pairs) = openai_codex_authorization_query(value) {
        return OpenAICodexAuthorizationInput {
            code: pairs
                .iter()
                .find(|(key, value)| key == "code" && !value.trim().is_empty())
                .map(|(_, value)| value.clone()),
            state: pairs
                .iter()
                .find(|(key, value)| key == "state" && !value.trim().is_empty())
                .map(|(_, value)| value.clone()),
        };
    }
    if let Some((code, state)) = value.split_once('#') {
        return OpenAICodexAuthorizationInput {
            code: non_empty_string(code),
            state: non_empty_string(state),
        };
    }
    if value.contains("code=") {
        let mut parsed = OpenAICodexAuthorizationInput {
            code: None,
            state: None,
        };
        for (key, value) in
            url::form_urlencoded::parse(value.trim_start_matches(['?', '&']).as_bytes())
        {
            match key.as_ref() {
                "code" => parsed.code = non_empty_string(&value),
                "state" => parsed.state = non_empty_string(&value),
                _ => {}
            }
        }
        return parsed;
    }
    OpenAICodexAuthorizationInput {
        code: Some(value.to_string()),
        state: None,
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}

fn openai_codex_authorization_error(input: &str) -> bool {
    openai_codex_authorization_query(input).is_some_and(|pairs| {
        pairs
            .iter()
            .any(|(key, value)| key == "error" && !value.trim().is_empty())
    })
}

fn openai_codex_manual_code(input: &str, expected_state: &str) -> Result<String, PiAiError> {
    if openai_codex_authorization_error(input) {
        return Err(
            "OpenAI Codex OAuth login failed [protocol]: authorization was denied or failed. Retry `/login openai-codex` or paste a fresh redirect URL."
                .into(),
        );
    }
    let parsed = parse_openai_codex_authorization_input(input);
    if parsed
        .state
        .as_deref()
        .is_some_and(|received| received != expected_state)
    {
        return Err(
            "OpenAI Codex OAuth login failed [protocol]: state mismatch. Retry `/login openai-codex` with a fresh browser flow."
                .into(),
        );
    }
    parsed.code.ok_or_else(|| {
        "OpenAI Codex OAuth login failed [protocol]: authorization code is missing. Retry `/login openai-codex` or paste the complete redirect URL."
            .into()
    })
}

fn openai_codex_state() -> String {
    let mut bytes = [0u8; 16];
    // Invariant: the OS random source failing is unrecoverable for OAuth.
    #[allow(clippy::expect_used)]
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .expect("system random should fill OAuth state");
    let mut state = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(state, "{byte:02x}");
    }
    state
}

fn openai_codex_jwt_account_id(access_token: &str) -> Result<String, PiAiError> {
    let mut parts = access_token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(PiAiError::jwt(
            "JWT must contain exactly three non-empty segments",
        ));
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(PiAiError::jwt(
            "JWT must contain exactly three non-empty segments",
        ));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .map_err(|_| PiAiError::jwt("JWT payload is not valid base64url"))?;
    let payload: Value = serde_json::from_slice(&decoded)
        .map_err(|_| PiAiError::jwt("JWT payload is not valid JSON"))?;
    let account_id = payload
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account_id| {
            !account_id.is_empty()
                && !account_id
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
        .ok_or("JWT payload has no valid ChatGPT account id")?;
    Ok(account_id.to_string())
}

#[derive(Debug, Clone)]
struct OpenAICodexToken {
    access: String,
    refresh: String,
    expires: u64,
}

#[derive(Debug, Clone)]
struct OpenAICodexError {
    kind: OAuthFailureKind,
    operation: &'static str,
    detail: String,
}

impl OpenAICodexError {
    fn new(kind: OAuthFailureKind, operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            operation,
            detail: detail.into(),
        }
    }

    fn cancelled(operation: &'static str) -> Self {
        Self::new(
            OAuthFailureKind::Cancelled,
            operation,
            DEVICE_CODE_CANCEL_MESSAGE,
        )
    }

    fn from_transport(operation: &'static str, error: PiAiError, secrets: &[&str]) -> Self {
        let error = error.to_string();
        let lower = error.to_ascii_lowercase();
        let kind = if lower == DEVICE_CODE_CANCEL_MESSAGE.to_ascii_lowercase()
            || lower.contains("login cancelled")
        {
            OAuthFailureKind::Cancelled
        } else if lower.contains("timed out") {
            OAuthFailureKind::Timeout
        } else {
            OAuthFailureKind::Network
        };
        Self::new(kind, operation, redact_secrets(&error, secrets))
    }

    fn from_status(
        operation: &'static str,
        status: reqwest::StatusCode,
        body: &str,
        secrets: &[&str],
    ) -> Self {
        let detail = openai_codex_error_detail(body, secrets).unwrap_or_else(|| status.to_string());
        let kind = openai_codex_status_kind(status, body);
        Self::new(
            kind,
            operation,
            format!("token {operation} failed ({status}): {detail}"),
        )
    }

    fn malformed(operation: &'static str, detail: &'static str) -> Self {
        Self::new(OAuthFailureKind::MalformedResponse, operation, detail)
    }

    #[allow(dead_code)] // superseded by account_message; kept for upstream-parity naming
    fn account(operation: &'static str, reason: &'static str) -> Self {
        Self::account_message(operation, reason.to_string())
    }

    fn account_message(operation: &'static str, reason: String) -> Self {
        Self::new(
            OAuthFailureKind::AccountExtraction,
            operation,
            format!("Failed to extract accountId from OpenAI Codex token: {reason}"),
        )
    }

    fn retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    fn render(&self) -> String {
        if self.kind == OAuthFailureKind::Cancelled {
            return DEVICE_CODE_CANCEL_MESSAGE.to_string();
        }
        let recovery = if self.kind.requires_relogin() {
            "Run `/login openai-codex` to re-authenticate."
        } else {
            "Retry the operation; if it persists, run `/login openai-codex` to re-authenticate."
        };
        format!(
            "OpenAI Codex OAuth {} failed [{}]: {}. {recovery}",
            self.operation,
            self.kind.code(),
            self.detail
        )
    }
}

fn openai_codex_error_code(body: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    let object = value.as_object()?;
    match object.get("error") {
        Some(Value::String(error)) => Some(error.to_string()),
        Some(Value::Object(error)) => error
            .get("code")
            .or_else(|| error.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => object
            .get("code")
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn openai_codex_status_kind(status: reqwest::StatusCode, body: &str) -> OAuthFailureKind {
    let code = openai_codex_error_code(body)
        .map(|code| code.to_ascii_lowercase())
        .unwrap_or_default();
    if code == "invalid_grant" || code.contains("invalid_grant") {
        return OAuthFailureKind::InvalidGrant;
    }
    if matches!(
        code.as_str(),
        "unauthorized" | "invalid_token" | "authentication_required"
    ) {
        return OAuthFailureKind::Unauthorized;
    }
    if matches!(
        code.as_str(),
        "rate_limited" | "rate_limit_exceeded" | "too_many_requests"
    ) {
        return OAuthFailureKind::RateLimited;
    }
    if matches!(code.as_str(), "server_error" | "temporarily_unavailable") {
        return OAuthFailureKind::Server;
    }
    match status.as_u16() {
        408 => OAuthFailureKind::Timeout,
        401 | 403 => OAuthFailureKind::Unauthorized,
        429 => OAuthFailureKind::RateLimited,
        500..=599 => OAuthFailureKind::Server,
        _ => OAuthFailureKind::Protocol,
    }
}

fn valid_openai_codex_token_value(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

fn openai_codex_expires_in(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0 && *seconds <= u64::MAX as f64)
            .map(|seconds| seconds as u64)
    })
}

async fn read_openai_codex_token_response(
    response: reqwest::Response,
    operation: &str,
    signal: Option<&AtomicBool>,
    secrets: &[&str],
) -> Result<OpenAICodexToken, OpenAICodexError> {
    let status = response.status();
    let body = response_text_with_optional_abort(
        response,
        signal,
        &format!("OpenAI Codex token {operation}"),
    )
    .await
    .map_err(|error| {
        OpenAICodexError::from_transport(
            if operation == "refresh" {
                "refresh"
            } else {
                "exchange"
            },
            error,
            secrets,
        )
    })?;
    if !status.is_success() {
        // Match Pi's useful provider error while never echoing an untrusted
        // response body (which may contain request material or a credential).
        return Err(OpenAICodexError::from_status(
            if operation == "refresh" {
                "refresh"
            } else {
                "exchange"
            },
            status,
            &body,
            secrets,
        ));
    }
    let json: Value = serde_json::from_str(&body).map_err(|_| {
        OpenAICodexError::malformed(
            if operation == "refresh" {
                "refresh"
            } else {
                "exchange"
            },
            "token response was not valid JSON",
        )
    })?;
    let access = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| valid_openai_codex_token_value(value));
    let refresh = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| valid_openai_codex_token_value(value));
    let expires_in = json.get("expires_in").and_then(openai_codex_expires_in);
    let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
        return Err(OpenAICodexError::malformed(
            if operation == "refresh" {
                "refresh"
            } else {
                "exchange"
            },
            "token response is missing required fields",
        ));
    };
    Ok(OpenAICodexToken {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: crate::types::now_ms().saturating_add(expires_in.saturating_mul(1000)),
    })
}

fn openai_codex_credentials_from_token(
    token: OpenAICodexToken,
    operation: &'static str,
) -> Result<OAuthCredential, OpenAICodexError> {
    let account_id = openai_codex_jwt_account_id(&token.access)
        .map_err(|reason| OpenAICodexError::account_message(operation, reason.to_string()))?;
    let mut extra = BTreeMap::new();
    extra.insert("accountId".to_string(), Value::String(account_id));
    Ok(OAuthCredential {
        refresh: token.refresh,
        access: token.access,
        expires: token.expires,
        extra,
    })
}

struct OpenAICodexCallbackServer {
    listener: Option<tokio::net::TcpListener>,
    state: String,
    redirect_uri: String,
}

impl OpenAICodexCallbackServer {
    async fn bind(host: &str, port: u16, state: String) -> Result<Self, PiAiError> {
        // Upstream Pi keeps the manual-code path usable when the fixed
        // callback port is occupied. Preserve that recovery behavior instead
        // of turning a local bind collision into a login-wide failure.
        let listener = tokio::net::TcpListener::bind((host, port)).await.ok();
        let actual_port = match listener.as_ref() {
            Some(listener) => listener
                .local_addr()
                .map_err(|error| format!("read OAuth callback address: {error}"))?
                .port(),
            None => port,
        };
        Ok(Self {
            listener,
            state,
            redirect_uri: format!("http://localhost:{actual_port}/auth/callback"),
        })
    }

    async fn wait_for_code(self, cancel: Arc<AtomicBool>) -> Result<Option<String>, PiAiError> {
        tokio::time::timeout(
            OPENAI_CODEX_BROWSER_TIMEOUT,
            self.wait_for_code_until_cancel(cancel),
        )
        .await
        .map_err(|_| PiAiError::timeout("OpenAI Codex OAuth login timed out"))?
    }

    async fn wait_for_code_until_cancel(
        self,
        cancel: Arc<AtomicBool>,
    ) -> Result<Option<String>, PiAiError> {
        let Some(listener) = self.listener else {
            return Ok(None);
        };
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.map_err(|error| format!("OAuth callback accept failed: {error}"))?;
                    let mut buffer = [0u8; 16 * 1024];
                    let read = read_callback_request(&mut stream, &mut buffer, &cancel).await?;
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let mut request_line = request.lines().next().unwrap_or("").split_whitespace();
                    let method = request_line.next().unwrap_or("");
                    let target = request_line.next().unwrap_or("");
                    let parsed = url::Url::parse(&format!("http://localhost{target}"));
                    let result = match parsed {
                        Ok(_) if method != "GET" => {
                            write_oauth_callback_response(&mut stream, 405, "Callback method not allowed.").await?;
                            None
                        }
                        Ok(url) if url.path() != "/auth/callback" => {
                            write_oauth_callback_response(&mut stream, 404, "Callback route not found.").await?;
                            None
                        }
                        Ok(url) => {
                            let received_state = url.query_pairs()
                                .find(|(key, _)| key == "state")
                                .map(|(_, value)| value.into_owned());
                            let has_error = url
                                .query_pairs()
                                .any(|(key, value)| key == "error" && !value.is_empty());
                            if received_state.as_deref() != Some(self.state.as_str()) {
                                write_oauth_callback_response(&mut stream, 400, "State mismatch.").await?;
                                None
                            } else if has_error {
                                write_oauth_callback_response(&mut stream, 400, "OpenAI authorization failed.").await?;
                                None
                            } else if let Some(code) = url.query_pairs()
                                .find(|(key, _)| key == "code")
                                .map(|(_, value)| value.into_owned())
                                .filter(|code| !code.is_empty())
                            {
                                write_oauth_callback_response(&mut stream, 200, "OpenAI authentication completed. You can close this window.").await?;
                                Some(code)
                            } else {
                                write_oauth_callback_response(&mut stream, 400, "Missing authorization code.").await?;
                                None
                            }
                        }
                        Err(_) => {
                            write_oauth_callback_response(&mut stream, 400, "Invalid callback request.").await?;
                            None
                        }
                    };
                    if result.is_some() {
                        return Ok(result);
                    }
                }
                _ = wait_for_oauth_cancel(cancel.clone()) => return Ok(None),
            }
        }
    }
}

async fn wait_for_oauth_cancel(cancel: Arc<AtomicBool>) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_optional_oauth_cancel(cancel: Option<Arc<AtomicBool>>) {
    match cancel {
        Some(cancel) => wait_for_oauth_cancel(cancel).await,
        None => std::future::pending::<()>().await,
    }
}

async fn write_oauth_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<(), PiAiError> {
    use tokio::io::AsyncWriteExt;
    let safe_message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>OpenAI authentication</title></head><body><h1>{safe_message}</h1></body></html>"
    );
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| format!("OAuth callback response failed: {error}"))?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// OpenAI Codex OAuth implementation. The endpoint and callback seams are
/// test-only in spirit but public so integration tests can prove the complete
/// browser/device transport without contacting OpenAI or storing real tokens.
type OpenAICodexRefreshLock = tokio::sync::Mutex<()>;
type OpenAICodexRefreshLocks = BTreeMap<[u8; 32], Weak<OpenAICodexRefreshLock>>;

#[derive(Clone)]
pub struct OpenAICodexOAuth {
    base_url: String,
    callback_host: String,
    callback_port: u16,
    /// OpenAI has this callback URI registered exactly. The loopback test
    /// seam leaves it unset so a fixture may use its selected callback port.
    registered_redirect_uri: Option<String>,
    /// Serialize refresh requests for the same credential even when a caller
    /// bypasses the app-level credential-store lock.  The map key is a
    /// one-way digest, so refresh tokens are not retained in the coordinator.
    refresh_locks: Arc<Mutex<OpenAICodexRefreshLocks>>,
}

impl OpenAICodexOAuth {
    pub fn new() -> Arc<Self> {
        let callback_host = std::env::var("PI_OAUTH_CALLBACK_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let configured_base_url = std::env::var("PI_OPENAI_CODEX_AUTH_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().trim_end_matches('/').to_string());
        let uses_configured_endpoint = configured_base_url.is_some();
        // A callback-port override is retained only alongside an explicitly
        // configured auth endpoint. The normal OpenAI flow always binds the
        // registered 1455 port; the paired endpoint/port seam is used by
        // local integration fixtures and intentionally does not claim the
        // production redirect URI.
        let callback_port = configured_base_url
            .as_ref()
            .and_then(|_| std::env::var("PI_OAUTH_CALLBACK_PORT").ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(OPENAI_CODEX_CALLBACK_PORT);
        Arc::new(Self {
            base_url: configured_base_url.unwrap_or_else(|| OPENAI_CODEX_AUTH_BASE_URL.to_string()),
            callback_host,
            callback_port,
            registered_redirect_uri: (!uses_configured_endpoint)
                .then(|| OPENAI_CODEX_REDIRECT_URI.to_string()),
            refresh_locks: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn with_base_url_and_callback(
        base_url: impl Into<String>,
        callback_host: impl Into<String>,
        callback_port: u16,
    ) -> Arc<Self> {
        Arc::new(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            callback_host: callback_host.into(),
            callback_port,
            registered_redirect_uri: None,
            refresh_locks: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    fn urls(&self) -> OpenAICodexUrls {
        openai_codex_urls(&self.base_url)
    }

    fn refresh_lock(&self, refresh_token: &str) -> Arc<OpenAICodexRefreshLock> {
        let key: [u8; 32] = Sha256::digest(refresh_token.as_bytes()).into();
        let mut locks = self
            .refresh_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn exchange_code(
        &self,
        client: &reqwest::Client,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, PiAiError> {
        let urls = self.urls();
        let request = client.post(urls.token_url).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ]);
        let response =
            request_with_optional_abort(request, Some(signal), "OpenAI Codex token exchange")
                .await
                .map_err(|error| {
                    OpenAICodexError::from_transport("exchange", error, &[code, verifier]).render()
                })?;
        let token =
            read_openai_codex_token_response(response, "exchange", Some(signal), &[code, verifier])
                .await
                .map_err(|error| PiAiError::other(error.render()))?;
        openai_codex_credentials_from_token(token, "exchange")
            .map_err(|error| PiAiError::other(error.render()))
    }

    async fn login_browser(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, PiAiError> {
        let (verifier, challenge) = generate_pkce();
        let state = openai_codex_state();
        let callback =
            OpenAICodexCallbackServer::bind(&self.callback_host, self.callback_port, state.clone())
                .await?;
        // The local bind host/port is not the OAuth registration contract.
        // In production the authorization request and token exchange must
        // both use OpenAI's registered localhost:1455 URI, even when the
        // listener host is overridden or the port is already occupied.
        let redirect_uri = self
            .registered_redirect_uri
            .clone()
            .unwrap_or_else(|| callback.redirect_uri.clone());
        let mut authorize = url::Url::parse(&self.urls().authorize_url)
            .map_err(|error| format!("invalid OpenAI Codex authorization URL: {error}"))?;
        authorize
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", OPENAI_CODEX_CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", OPENAI_CODEX_SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "pi");
        interaction.notify(&AuthEvent::AuthUrl {
            url: authorize.to_string(),
            instructions: Some(
                "A browser window should open. Complete login to finish.".to_string(),
            ),
        });

        let manual_abort = Arc::new(AtomicBool::new(false));
        let callback_cancel = Arc::new(AtomicBool::new(false));
        let external_cancel = interaction.signal();
        let manual_prompt = AuthPrompt::ManualCode {
            message: "Complete login in your browser, or paste the authorization code / redirect URL here:".to_string(),
            placeholder: Some(redirect_uri.clone()),
        };
        let mut callback_future = Box::pin(callback.wait_for_code(callback_cancel.clone()));
        let mut manual_future =
            interaction.prompt_async_with_abort(&manual_prompt, manual_abort.clone());
        let external_cancel_future = wait_for_optional_oauth_cancel(external_cancel.clone());
        tokio::pin!(external_cancel_future);
        let code = tokio::select! {
            result = &mut callback_future => {
                match result? {
                    Some(code) => {
                        manual_abort.store(true, Ordering::SeqCst);
                        let _ = tokio::time::timeout(Duration::from_secs(1), &mut manual_future).await;
                        code
                    }
                    // A bind failure is represented by an immediately
                    // empty callback result, exactly so manual paste can
                    // recover as it does in upstream Pi.
                    None => {
                        let input = manual_future.await?;
                        openai_codex_manual_code(&input, &state)?
                    }
                }
            }
            result = &mut manual_future => {
                callback_cancel.store(true, Ordering::SeqCst);
                let input = result?;
                openai_codex_manual_code(&input, &state)?
            }
            _ = &mut external_cancel_future => {
                callback_cancel.store(true, Ordering::SeqCst);
                manual_abort.store(true, Ordering::SeqCst);
                return Err(PiAiError::LoginCancelled);
            }
        };
        let signal = external_cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        self.exchange_code(
            &reqwest::Client::new(),
            &code,
            &verifier,
            &redirect_uri,
            signal.as_ref(),
        )
        .await
    }

    async fn login_device(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, PiAiError> {
        let urls = self.urls();
        let client = reqwest::Client::new();
        let external_cancel = interaction
            .signal()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        if external_cancel.load(Ordering::SeqCst) {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
        }
        let request = client
            .post(&urls.device_user_code_url)
            .json(&serde_json::json!({"client_id": OPENAI_CODEX_CLIENT_ID}));
        let response = request_with_optional_abort(
            request,
            Some(external_cancel.as_ref()),
            "OpenAI Codex device code",
        )
        .await?;
        let status = response.status();
        let body = response_text_with_optional_abort(
            response,
            Some(external_cancel.as_ref()),
            "OpenAI Codex device code",
        )
        .await?;
        if status.as_u16() == 404 {
            return Err(PiAiError::invalid_response("OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."));
        }
        if !status.is_success() {
            return Err(PiAiError::other(openai_codex_status_error(
                "device code request",
                status,
                &body,
                &[],
            )));
        }
        let json: Value = serde_json::from_str(&body).map_err(|_| {
            PiAiError::invalid_response("Invalid OpenAI Codex device code response")
        })?;
        let device_auth_id = json
            .get("device_auth_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let user_code = json
            .get("user_code")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let interval_seconds = json.get("interval").and_then(|value| {
            value.as_f64().or_else(|| {
                value
                    .as_str()
                    .and_then(|value| value.trim().parse::<f64>().ok())
            })
        });
        let (Some(device_auth_id), Some(user_code), Some(interval_seconds)) =
            (device_auth_id, user_code, interval_seconds)
        else {
            return Err(PiAiError::invalid_response(
                "Invalid OpenAI Codex device code response fields",
            ));
        };
        if !interval_seconds.is_finite() || interval_seconds < 0.0 {
            return Err(PiAiError::invalid_response(
                "Invalid OpenAI Codex device code interval",
            ));
        }
        interaction.notify(&AuthEvent::DeviceCode {
            user_code: user_code.to_string(),
            verification_uri: urls.device_verification_uri.clone(),
            interval_seconds: Some(interval_seconds),
            expires_in_seconds: Some(OPENAI_CODEX_DEVICE_TIMEOUT_SECONDS),
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let poll_cancel = cancel.clone();
        let request_cancel = cancel.clone();
        let device_auth_id = device_auth_id.to_string();
        let user_code = user_code.to_string();
        let device_token_url = urls.device_token_url.clone();
        let client_for_poll = client.clone();
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            let client = client_for_poll.clone();
            let device_token_url = device_token_url.clone();
            let device_auth_id = device_auth_id.clone();
            let user_code = user_code.clone();
            let request_cancel = request_cancel.clone();
            let device_auth_id_for_error = device_auth_id.clone();
            let user_code_for_error = user_code.clone();
            Box::pin(async move {
                let request = client.post(device_token_url).json(&serde_json::json!({
                    "device_auth_id": device_auth_id,
                    "user_code": user_code,
                }));
                let response = match request_with_optional_abort(
                    request,
                    Some(request_cancel.as_ref()),
                    "OpenAI Codex device auth",
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        return DeviceCodePollResult::Failed {
                            message: format!("OpenAI Codex device auth request failed: {error}"),
                        }
                    }
                };
                let status = response.status();
                let body = match response_text_with_optional_abort(
                    response,
                    Some(request_cancel.as_ref()),
                    "OpenAI Codex device auth",
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => {
                        return DeviceCodePollResult::Failed {
                            message: format!(
                                "OpenAI Codex device auth response read failed: {error}"
                            ),
                        }
                    }
                };
                if status.is_success() {
                    let json: Value = match serde_json::from_str(&body) {
                        Ok(json) => json,
                        Err(_) => {
                            return DeviceCodePollResult::Failed {
                                message: "Invalid OpenAI Codex device auth token response"
                                    .to_string(),
                            }
                        }
                    };
                    let authorization_code = json
                        .get("authorization_code")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty());
                    let code_verifier = json
                        .get("code_verifier")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty());
                    return match (authorization_code, code_verifier) {
                        (Some(authorization_code), Some(code_verifier)) => {
                            DeviceCodePollResult::Complete((
                                authorization_code.to_string(),
                                code_verifier.to_string(),
                            ))
                        }
                        _ => DeviceCodePollResult::Failed {
                            message: "Invalid OpenAI Codex device auth token response fields"
                                .to_string(),
                        },
                    };
                }
                if status.as_u16() == 403 || status.as_u16() == 404 {
                    return DeviceCodePollResult::Pending;
                }
                let error_code = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|json| json.get("error").cloned())
                    .and_then(|error| match error {
                        Value::Object(object) => object.get("code").cloned(),
                        other => Some(other),
                    })
                    .and_then(|error| error.as_str().map(str::to_string));
                match error_code.as_deref() {
                    Some("deviceauth_authorization_pending") => DeviceCodePollResult::Pending,
                    Some("slow_down") => DeviceCodePollResult::SlowDown {
                        interval_seconds: None,
                    },
                    _ => DeviceCodePollResult::Failed {
                        message: openai_codex_status_error(
                            "device auth",
                            status,
                            &body,
                            &[
                                device_auth_id_for_error.as_str(),
                                user_code_for_error.as_str(),
                            ],
                        ),
                    },
                }
            })
        }));
        options.interval_seconds = Some(interval_seconds);
        options.expires_in_seconds = Some(OPENAI_CODEX_DEVICE_TIMEOUT_SECONDS);
        options.signal = Some(poll_cancel);

        let poll_future = poll_oauth_device_code_flow(&mut options);
        tokio::pin!(poll_future);
        let authorization = if interaction.supports_async_prompt() {
            let cancel_prompt = AuthPrompt::Text {
                message: "Waiting for OpenAI authorization. Press Esc to cancel.".to_string(),
                placeholder: None,
            };
            let mut cancel_future =
                interaction.prompt_async_with_abort(&cancel_prompt, cancel.clone());
            let external_cancel_future =
                wait_for_optional_oauth_cancel(Some(external_cancel.clone()));
            tokio::pin!(external_cancel_future);
            tokio::select! {
                result = &mut poll_future => {
                    let result = result?;
                    cancel.store(true, Ordering::SeqCst);
                    // The prompt is instructed to stop through `cancel`, but
                    // a terminal UI is not allowed to hold a successful OAuth
                    // login hostage if it is non-cooperative. Dropping the
                    // future also releases any prompt-owned resources; the
                    // interaction contract remains responsible for observing
                    // the abort flag in its own UI task.
                    result
                },
                result = &mut cancel_future => {
                    cancel.store(true, Ordering::SeqCst);
                    let _ = result;
                    return Err(PiAiError::LoginCancelled);
                },
                _ = &mut external_cancel_future => {
                    cancel.store(true, Ordering::SeqCst);
                    return Err(PiAiError::LoginCancelled);
                }
            }
        } else {
            let external_cancel_future =
                wait_for_optional_oauth_cancel(Some(external_cancel.clone()));
            tokio::pin!(external_cancel_future);
            tokio::select! {
                result = &mut poll_future => result?,
                _ = &mut external_cancel_future => {
                    cancel.store(true, Ordering::SeqCst);
                    return Err(PiAiError::LoginCancelled);
                }
            }
        };
        let (authorization_code, code_verifier) = authorization;
        self.exchange_code(
            &client,
            &authorization_code,
            &code_verifier,
            &urls.device_redirect_uri,
            external_cancel.as_ref(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl OAuthAuth for OpenAICodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("ChatGPT Plus/Pro")
    }

    async fn login(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, PiAiError> {
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
        }
        let method = interaction.prompt(&AuthPrompt::Select {
            message: "Select OpenAI Codex login method:".to_string(),
            options: vec![
                AuthSelectOption {
                    id: "browser".to_string(),
                    label: "Browser login (default)".to_string(),
                    description: None,
                },
                AuthSelectOption {
                    id: "device_code".to_string(),
                    label: "Device code login (headless)".to_string(),
                    description: None,
                },
            ],
        })?;
        if interaction
            .signal()
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::SeqCst))
        {
            return Err(PiAiError::other(DEVICE_CODE_CANCEL_MESSAGE));
        }
        match method.as_str() {
            "browser" => self.login_browser(interaction).await,
            "device_code" => self.login_device(interaction).await,
            _ => Err(PiAiError::invalid_response(format!(
                "Unknown OpenAI Codex login method: {method}",
            ))),
        }
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AtomicBool,
    ) -> Result<OAuthCredential, PiAiError> {
        if signal.load(Ordering::SeqCst) {
            return Err(OpenAICodexError::cancelled("refresh").render().into());
        }

        let refresh_lock = self.refresh_lock(&credential.refresh);
        let _refresh_guard = tokio::select! {
            guard = refresh_lock.lock() => guard,
            _ = wait_for_atomic_abort_ref(signal) => {
                return Err(OpenAICodexError::cancelled("refresh").render().into());
            }
        };
        let client = reqwest::Client::new();
        let mut attempt = 0u8;
        loop {
            if signal.load(Ordering::SeqCst) {
                return Err(OpenAICodexError::cancelled("refresh").render().into());
            }
            let request = client.post(self.urls().token_url).form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", credential.refresh.as_str()),
                ("client_id", OPENAI_CODEX_CLIENT_ID),
            ]);
            let result = match request_with_optional_abort(
                request,
                Some(signal),
                "OpenAI Codex token refresh",
            )
            .await
            {
                Ok(response) => {
                    let token = read_openai_codex_token_response(
                        response,
                        "refresh",
                        Some(signal),
                        &[credential.refresh.as_str(), credential.access.as_str()],
                    )
                    .await;
                    match token {
                        Ok(token) => openai_codex_credentials_from_token(token, "refresh"),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(OpenAICodexError::from_transport(
                    "refresh",
                    error,
                    &[credential.refresh.as_str(), credential.access.as_str()],
                )),
            };

            match result {
                Ok(credentials) => return Ok(credentials),
                Err(error) if attempt == 0 && error.retryable() => {
                    attempt += 1;
                    abortable_sleep_ref(50, signal, DEVICE_CODE_CANCEL_MESSAGE)
                        .await
                        .map_err(|_| {
                            PiAiError::other(OpenAICodexError::cancelled("refresh").render())
                        })?;
                }
                Err(error) => return Err(PiAiError::other(error.render())),
            }
        }
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
        assert_eq!(err.to_string(), DEVICE_CODE_CANCEL_MESSAGE);
    }

    #[tokio::test]
    async fn timeout_when_all_pending() {
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::<()>::Pending })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(2);
        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();
        assert_eq!(err.to_string(), DEVICE_CODE_TIMEOUT_MESSAGE);
    }

    #[tokio::test]
    async fn expired_device_flow_does_not_start_a_poll_request() {
        use std::sync::atomic::AtomicUsize;

        let poll_count = Arc::new(AtomicUsize::new(0));
        let poll_count_for_callback = poll_count.clone();
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            poll_count_for_callback.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { DeviceCodePollResult::<()>::Complete(()) })
        }));
        options.expires_in_seconds = Some(0);

        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();

        assert_eq!(err.to_string(), DEVICE_CODE_TIMEOUT_MESSAGE);
        assert_eq!(poll_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn base64url_no_padding() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"f\xfb"), "Zvs");
        assert_eq!(base64url_encode(b"f\xfb\xff"), "Zvv_");
    }

    #[test]
    fn negative_provider_interval_is_clamped_to_the_rfc_minimum() {
        assert_eq!(
            interval_to_ms(Some(-1.0), 5_000),
            DEVICE_CODE_MINIMUM_INTERVAL_MS
        );
    }

    #[test]
    fn retry_after_http_dates_are_parsed_and_invalid_dates_rejected() {
        assert_eq!(
            parse_http_date_delay_ms_at("Thu, 01 Jan 1970 00:00:02 GMT", 0),
            Some(2_000)
        );
        assert_eq!(
            parse_http_date_delay_ms_at("Thu, 01 Jan 1970 00:00:02 GMT", 3_000),
            Some(0)
        );
        assert_eq!(
            parse_http_date_delay_ms_at("Thu, 30 Feb 1970 00:00:02 GMT", 0),
            None
        );
        assert_eq!(parse_http_date_delay_ms_at("not a date", 0), None);
    }
}
