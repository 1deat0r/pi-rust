//! Google Vertex AI adaptor — port of `packages/ai/src/api/google-vertex.ts`.
//!
//! Uses the same `GenerateContentRequest`/SSE surface as the Gemini adaptor
//! (`crates/pi-ai/src/api/google_generative_ai.rs`) against the Vertex
//! endpoint:
//!
//!   `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/
//!    locations/{location}/publishers/google/models/{model}:streamGenerateContent`
//!
//! Auth is either an explicit Google Cloud API key (`x-goog-api-key`) or
//! Application Default Credentials (`Authorization: Bearer <token>`).
//!
//! ADC file auth supports both service-account JWT exchange and authorized-user
//! refresh-token exchange, including the file's token URI and configured
//! scopes. Ambient metadata-server and workload-identity resolution are not
//! ported.

//! The provider facade still requires project and location for ADC. Explicit
//! Vertex API keys use the request path selected by the adaptor without
//! acquiring an ADC token.

//! The credential-file implementation deliberately stops at the file-based
//! ADC sources; gcloud CLI, metadata-server, and external-account discovery
//! remain outside this adaptor's scope.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{clamp_thinking_level, Model};
use crate::sse::SseParser;
use crate::types::ModelThinkingLevel;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason, SimpleStreamOptions,
    StopReason, StreamOptions, ToolChoice, Usage,
};

use super::google_generative_ai::{
    build_params, extract_google_error, process_google_events, GoogleOptions, GoogleThinking,
};
use super::google_shared::{resolve_google_thinking_level, ResolvedGoogleThinkingLevel};
use super::openai_completions::{
    abortable, apply_payload_hook, error_reason, immediate_error_stream, signal_aborted,
    terminal_error_message,
};

const API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
pub const VERTEX_ADC_DEFAULT_PATH: &str = "~/.config/gcloud/application_default_credentials.json";
const DEFAULT_ADC_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_ADC_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Options for Vertex requests (subset of upstream `GoogleVertexOptions`).
#[derive(Clone, Default)]
pub struct GoogleVertexOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<String>,
    pub thinking: Option<GoogleThinking>,
    pub project: Option<String>,
    pub location: Option<String>,
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

fn get_provider_env_value(name: &str, env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    super::openai_completions::get_provider_env_value(name, env)
}

/// A Vertex API key is an explicit key unless it is empty, the
/// `gcp-vertex-credentials` sentinel, or a `<...>` placeholder (upstream
/// `resolveApiKey`).
pub fn resolve_api_key(api_key: Option<&str>) -> Option<String> {
    let api_key = api_key.map(|s| s.trim()).unwrap_or("");
    if api_key.is_empty()
        || api_key == GCP_VERTEX_CREDENTIALS_MARKER
        || is_placeholder_api_key(api_key)
    {
        return None;
    }
    Some(api_key.to_string())
}

fn is_placeholder_api_key(api_key: &str) -> bool {
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"^<[^>]+>$").unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    PLACEHOLDER.is_match(api_key)
}

/// Resolve the project id: options.project, `GOOGLE_CLOUD_PROJECT`, or
/// `GCLOUD_PROJECT` (upstream `resolveProject`).
pub fn resolve_project(
    project: Option<&str>,
    env: Option<&crate::types::ProviderEnv>,
) -> Result<String, String> {
    let project = nonempty_value(project)
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_PROJECT", env))
        .or_else(|| get_provider_env_value("GCLOUD_PROJECT", env));
    project.ok_or_else(|| {
        "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
            .to_string()
    })
}

/// Resolve the location: options.location or `GOOGLE_CLOUD_LOCATION`
/// (upstream `resolveLocation`).
pub fn resolve_location(
    location: Option<&str>,
    env: Option<&crate::types::ProviderEnv>,
) -> Result<String, String> {
    let location =
        nonempty_value(location).or_else(|| get_provider_env_value("GOOGLE_CLOUD_LOCATION", env));
    location.ok_or_else(|| {
        "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
            .to_string()
    })
}

fn nonempty_value(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

/// `resolveCustomBaseUrl`: a custom base URL is used unless empty or still
/// carrying the `{location}` placeholder.
pub fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() || trimmed.contains("{location}") {
        return None;
    }
    Some(trimmed.to_string())
}

/// `baseUrlIncludesApiVersion`: the base path contains a `vNbetaM`-style
/// version segment.
pub fn base_url_includes_api_version(base_url: &str) -> bool {
    static VERSION_PART: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"^v\d+(?:beta\d*)?$")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    static VERSION_PATH: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"(?:^|/)v\d+(?:beta\d*)?(?:/|$)")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    let path_has_version = base_url.split('/').any(|part| VERSION_PART.is_match(part));
    if path_has_version {
        return true;
    }
    VERSION_PATH.is_match(base_url)
}

/// Compute the URL + headers for a Vertex `:streamGenerateContent` request.
/// Returns (url, headers).
fn build_request(
    model: &Model,
    options: &GoogleVertexOptions,
    project: Option<&str>,
    location: Option<&str>,
    api_key: Option<&str>,
    bearer_token: Option<&str>,
    api_version: &str,
) -> (String, Vec<(String, String)>) {
    let custom_base = resolve_custom_base_url(&model.base_url);
    let version_segment = match &custom_base {
        Some(base) if base_url_includes_api_version(base) => String::new(),
        _ => format!("/{api_version}"),
    };
    let base = custom_base
        .unwrap_or_else(|| {
            location
                .map(|location| format!("https://{location}-aiplatform.googleapis.com"))
                .unwrap_or_else(|| "https://aiplatform.googleapis.com".to_string())
        })
        .trim_end_matches('/')
        .to_string();
    let resource = match (project, location) {
        (Some(project), Some(location)) => {
            format!("/projects/{project}/locations/{location}")
        }
        _ => String::new(),
    };
    let url = format!(
        "{base}{version_segment}{resource}/publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.id
    );

    // Headers: User-Agent default, then model.headers / options headers merge
    // (upstream `providerHeadersToRecord({ "User-Agent": pi, ...model.headers,
    // ...optionsHeaders })`).
    let mut headers: Vec<(String, String)> = vec![("User-Agent".to_string(), pi_user_agent())];
    if let Some(model_headers) = &model.headers {
        for (k, v) in model_headers {
            headers.push((k.clone(), v.clone()));
        }
    }
    if let Some(options_headers) = &options.base.base.headers {
        for (k, v) in options_headers {
            // `providerHeadersToRecord({ ...model.headers, ...optionsHeaders })`
            // drops null values after the spread.  Remove the inherited
            // header even when the option is an explicit suppression.
            headers.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
            if let Some(v) = v {
                headers.push((k.clone(), v.clone()));
            }
        }
    }
    headers.retain(|(k, _)| !k.is_empty());
    if let Some(key) = api_key {
        headers.push(("x-goog-api-key".to_string(), key.to_string()));
    }
    if let Some(token) = bearer_token {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    (url, headers)
}

fn pi_user_agent() -> String {
    static USER_AGENT: OnceLock<String> = OnceLock::new();
    USER_AGENT
        .get_or_init(|| format!("pi ({} {}; {})", node_platform(), os_release(), node_arch()))
        .clone()
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        platform => platform,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        arch => arch,
    }
}

fn os_release() -> String {
    #[cfg(unix)]
    {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|release| release.trim().to_string())
            .filter(|release| !release.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(not(unix))]
    {
        "unknown".to_string()
    }
}

fn is_gemini3_pro_model(id: &str) -> bool {
    static GEMINI3_PRO: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"(?i)gemini-3(?:\.\d+)?-pro")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    GEMINI3_PRO.is_match(id)
}

fn is_gemini3_flash_model(id: &str) -> bool {
    static GEMINI3_FLASH: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Compile-time literal; a failure is a build defect.
        #[allow(clippy::panic)]
        regex::Regex::new(r"gemini-3(?:\.\d+)?-flash")
            .unwrap_or_else(|error| panic!("static regex: {error}"))
    });
    let id = id.to_lowercase();
    GEMINI3_FLASH.is_match(&id) || id == "gemini-flash-latest" || id == "gemini-flash-lite-latest"
}

/// Apply a configured thinking level or budget to the options (upstream
/// `getGemini3ThinkingLevel` / `getGoogleBudget`).
fn thinking_for_level(
    model_id: &str,
    level: ResolvedGoogleThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> GoogleThinking {
    let gemini3 = is_gemini3_pro_model(model_id) || is_gemini3_flash_model(model_id);
    if gemini3 {
        GoogleThinking {
            enabled: true,
            budget_tokens: None,
            level: Some(super::google_generative_ai::google_thinking_level(
                level, model_id,
            )),
        }
    } else {
        GoogleThinking {
            enabled: true,
            budget_tokens: Some(super::google_generative_ai::google_budget(
                model_id,
                level,
                custom_budgets,
            )),
            level: None,
        }
    }
}

/// Convert the Vertex options to the shared Gemini option shape for request
/// building (the request body is identical).
fn to_google_options(options: &GoogleVertexOptions) -> GoogleOptions {
    GoogleOptions {
        base: options.base.clone(),
        tool_choice: options.tool_choice.clone(),
        thinking: options.thinking.clone(),
    }
}

enum GoogleRequestError {
    Aborted,
    Transport(String),
    RetryDelay(String),
}

/// Execute a fresh Vertex HTTP request for each retry. The upstream Google
/// SDK request is wrapped in `retryGoogleRequest`; the REST transport needs to
/// apply the same policy to HTTP responses because reqwest does not turn a
/// retryable status into an error for us.
async fn send_google_request(
    client: &reqwest::Client,
    endpoint: &str,
    params: &Value,
    headers: &[(String, String)],
    options: &GoogleVertexOptions,
) -> Result<reqwest::Response, GoogleRequestError> {
    let max_retries = options.base.base.max_retries.unwrap_or(0);
    let max_retry_delay_ms = options.base.base.max_retry_delay_ms;
    let timeout_ms = options.base.base.timeout_ms;
    let signal = options.base.abort_signal.clone();
    let mut retry_index = 0;

    loop {
        let mut request = client
            .post(endpoint)
            .header("content-type", "application/json");
        if let Some(timeout_ms) = timeout_ms {
            request = request.timeout(Duration::from_millis(timeout_ms));
        }
        request = request.json(params);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = match abortable(request.send(), signal.clone()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if retry_index >= max_retries {
                    return Err(GoogleRequestError::Transport(format!(
                        "Request failed: {error}"
                    )));
                }
                let delay = super::openai_completions::exponential_retry_delay(retry_index);
                if abortable(
                    tokio::time::sleep(Duration::from_millis(delay)),
                    signal.clone(),
                )
                .await
                .is_err()
                {
                    return Err(GoogleRequestError::Aborted);
                }
                retry_index += 1;
                continue;
            }
            Err(_) => return Err(GoogleRequestError::Aborted),
        };

        let status = response.status().as_u16();
        let should_retry = super::openai_completions::retryable_provider_status(
            status,
            response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok()),
        );
        if retry_index >= max_retries || !should_retry {
            return Ok(response);
        }

        let delay = match super::openai_completions::retry_after_delay_ms(response.headers()) {
            Some(delay) => {
                let max_delay = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
                if max_delay > 0 && delay > max_delay {
                    let provider_message = match abortable(response.bytes(), signal.clone()).await {
                        Ok(Ok(body)) => {
                            let detail = extract_google_error(&String::from_utf8_lossy(&body));
                            format!("Google API error ({status}): {detail}")
                        }
                        Ok(Err(error)) => format!("Request body failed: {error}"),
                        Err(_) => return Err(GoogleRequestError::Aborted),
                    };
                    return Err(GoogleRequestError::RetryDelay(format!(
                        "Server requested {}s retry delay (max: {}s). {provider_message}",
                        delay.div_ceil(1000),
                        max_delay.div_ceil(1000),
                    )));
                }
                delay
            }
            None => super::openai_completions::exponential_retry_delay(retry_index),
        };

        drop(response);
        if abortable(
            tokio::time::sleep(Duration::from_millis(delay)),
            signal.clone(),
        )
        .await
        .is_err()
        {
            return Err(GoogleRequestError::Aborted);
        }
        retry_index += 1;
    }
}

/// Stream a request against the Vertex AI endpoint.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &GoogleVertexOptions,
) -> AssistantMessageEventStream {
    if signal_aborted(options.base.abort_signal.as_ref()) {
        return immediate_error_stream(model, "Request was aborted", true);
    }
    let stream = AssistantMessageEventStream::new();
    let sender = match stream.sender() {
        Some(s) => s,
        None => return stream,
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let api_key = api_key.map(|s| s.to_string());

    tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);

        let result = async {
            if signal_aborted(options.base.abort_signal.as_ref()) {
                return Err("Request was aborted".to_string());
            }
            // Resolve API key with marker/placeholder fallback to ADC.
            let store_api_key: Option<String> =
                api_key.as_deref().and_then(|k| resolve_api_key(Some(k)));
            // Options headers may carry the resolved key via the auth facade;
            // a placeholdered apiKey must not disable a real header key.
            let options_key = options
                .base
                .base
                .headers
                .as_ref()
                .and_then(|h| h.get("x-goog-api-key"))
                .and_then(|v| v.clone());
            let api_key = store_api_key.or_else(|| {
                options_key
                    .as_deref()
                    .and_then(|k| resolve_api_key(Some(k)))
            });

            let (project, location, bearer) = if api_key.is_none() {
                let project =
                    resolve_project(options.project.as_deref(), options.base.base.env.as_ref())?;
                let location =
                    resolve_location(options.location.as_deref(), options.base.base.env.as_ref())?;
                let bearer = abortable(
                    resolve_adc_access_token(&client, options.base.base.env.as_ref()),
                    options.base.abort_signal.clone(),
                )
                .await
                .map_err(|_| "Request was aborted".to_string())??;
                (Some(project), Some(location), Some(bearer))
            } else {
                (None, None, None)
            };

            let params = build_params(&model, &context, &to_google_options(&options))?;
            let params = apply_payload_hook(
                params,
                &model,
                options.base.on_payload.as_ref(),
                options.base.abort_signal.clone(),
            )
            .await
            .map_err(|_| "Request was aborted".to_string())?;

            let (endpoint, headers) = build_request(
                &model,
                &options,
                project.as_deref(),
                location.as_deref(),
                api_key.as_deref(),
                bearer.as_deref(),
                API_VERSION,
            );

            let response =
                match send_google_request(&client, &endpoint, &params, &headers, &options).await {
                    Ok(response) => response,
                    Err(GoogleRequestError::Aborted) => {
                        return Err("Request was aborted".to_string())
                    }
                    Err(GoogleRequestError::Transport(error))
                    | Err(GoogleRequestError::RetryDelay(error)) => return Err(error),
                };
            let status = response.status();
            let provider_response = crate::types::ProviderResponse {
                status: status.as_u16(),
                headers: crate::utils::response_headers(response.headers()),
            };
            if let Some(on_response) = &options.base.on_response {
                on_response(&provider_response, &model);
            }
            let body = abortable(response.bytes(), options.base.abort_signal.clone())
                .await
                .map_err(|_| "Request was aborted".to_string())?
                .map_err(|err| format!("Request body failed: {err}"))?;
            if signal_aborted(options.base.abort_signal.as_ref()) {
                return Err("Request was aborted".to_string());
            }
            if !status.is_success() {
                let body_text = String::from_utf8_lossy(&body).to_string();
                let detail = extract_google_error(&body_text);
                return Err(format!(
                    "Google API error ({}): {}",
                    status.as_u16(),
                    detail
                ));
            }
            Ok::<_, String>((body.to_vec(), status.as_u16()))
        }
        .await;

        match result {
            Ok((body, _status)) => {
                let body_text = String::from_utf8_lossy(&body).to_string();
                let events = SseParser::parse_text(&body_text);
                pusher.push(AssistantMessageEvent::Start {
                    partial: new_output(&model),
                });
                match process_google_events(&model, &events, |event| pusher.push(event)) {
                    Ok(message) => {
                        if signal_aborted(options.base.abort_signal.as_ref()) {
                            let message =
                                terminal_error_message(&model, "Request was aborted", true);
                            pusher.push(AssistantMessageEvent::Error {
                                reason: ErrorReason::Aborted,
                                error_message: message.clone(),
                            });
                            pusher.end(Some(message));
                            return;
                        }
                        let reason = match message.stop_reason().unwrap_or(StopReason::Stop) {
                            StopReason::Stop => DoneReason::Stop,
                            StopReason::Length => DoneReason::Length,
                            StopReason::ToolUse => DoneReason::ToolUse,
                            StopReason::Deferred => DoneReason::Deferred,
                            _ => DoneReason::Stop,
                        };
                        pusher.push(AssistantMessageEvent::Done {
                            reason,
                            message: message.clone(),
                        });
                        pusher.end(Some(message));
                    }
                    Err(err) => {
                        let mut message = new_output(&model);
                        message.set_stop_reason(StopReason::Error);
                        super::anthropic_messages::set_error_message(&mut message, err);
                        pusher.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error_message: message.clone(),
                        });
                        pusher.end(Some(message));
                    }
                }
            }
            Err(err) => {
                let aborted = signal_aborted(options.base.abort_signal.as_ref());
                let message = terminal_error_message(
                    &model,
                    if aborted {
                        "Request was aborted".to_string()
                    } else {
                        err
                    },
                    aborted,
                );
                pusher.push(AssistantMessageEvent::Error {
                    reason: error_reason(aborted),
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
            }
        }
    });
    stream
}

/// `streamSimple`: resolves reasoning to a Vertex thinking config and
/// forwards to `stream` (upstream `streamSimple`).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let base = GoogleVertexOptions {
        base: options.base.clone(),
        tool_choice: options.tool_choice.as_ref().map(|t| match t {
            ToolChoice::Auto => "auto".into(),
            ToolChoice::None => "none".into(),
        }),
        thinking: None,
        project: None,
        location: None,
    };
    if options.reasoning.is_none() {
        return stream(
            model,
            context,
            client,
            api_key,
            &GoogleVertexOptions {
                thinking: Some(GoogleThinking {
                    enabled: false,
                    budget_tokens: None,
                    level: None,
                }),
                ..base
            },
        );
    }

    #[allow(clippy::expect_used)] // invariant: callers resolve reasoning before this path
    let reasoning = options
        .reasoning
        .expect("reasoning resolved before thinking clamp");
    let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(reasoning));
    let resolved = resolve_google_thinking_level(clamped, model);
    let thinking = thinking_for_level(&model.id, resolved, options.thinking_budgets.as_ref());
    stream(
        model,
        context,
        client,
        api_key,
        &GoogleVertexOptions {
            thinking: Some(thinking),
            ..base
        },
    )
}

// ---------------------------------------------------------------------------
// Application Default Credentials (file-based sources)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ServiceAccountCredentials {
    client_email: String,
    private_key: String,
    token_uri: String,
    scopes: Vec<String>,
}

#[derive(Debug, Clone)]
struct AuthorizedUserCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: String,
    scopes: Vec<String>,
}

#[derive(Debug, Clone)]
enum AdcCredentials {
    ServiceAccount(ServiceAccountCredentials),
    AuthorizedUser(AuthorizedUserCredentials),
}

/// Resolve the ADC path from an explicit credentials path or the standard
/// gcloud home. An explicit path wins even when it does not exist; callers can
/// then report the missing file instead of silently falling back.
pub fn resolve_adc_path(explicit_path: Option<&str>, home: Option<&str>) -> String {
    if let Some(path) = explicit_path.filter(|path| !path.trim().is_empty()) {
        return path.to_string();
    }
    if let Some(home) = home.filter(|home| !home.trim().is_empty()) {
        return format!(
            "{}/.config/gcloud/application_default_credentials.json",
            home.trim_end_matches('/')
        );
    }
    VERTEX_ADC_DEFAULT_PATH.to_string()
}

fn find_adc_path(env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    let explicit_path = get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env);
    let home = get_provider_env_value("HOME", env);
    let path = resolve_adc_path(explicit_path.as_deref(), home.as_deref());
    let path = expand_home(&path, home.as_deref());
    Path::new(&path).exists().then_some(path)
}

fn expand_home(path: &str, home: Option<&str>) -> String {
    path.strip_prefix("~/")
        .and_then(|rest| {
            home.map(str::to_string)
                .or_else(|| std::env::var("HOME").ok())
                .map(|home| format!("{home}/{rest}"))
        })
        .unwrap_or_else(|| path.to_string())
}

fn required_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("ADC file missing {field}"))
}

fn token_uri(value: &Value) -> String {
    value
        .get("token_uri")
        .and_then(Value::as_str)
        .filter(|uri| !uri.trim().is_empty())
        .unwrap_or(DEFAULT_ADC_TOKEN_URI)
        .to_string()
}

fn configured_scopes(value: &Value, default: &[&str]) -> Vec<String> {
    let mut scopes = value
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .flat_map(str::split_whitespace)
                .filter(|scope| !scope.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if scopes.is_empty() {
        scopes = value
            .get("scope")
            .and_then(Value::as_str)
            .map(|scope| {
                scope
                    .split_whitespace()
                    .filter(|scope| !scope.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
    }
    if scopes.is_empty() {
        scopes.extend(default.iter().map(|scope| (*scope).to_string()));
    }
    scopes
}

fn read_adc_credentials(path: &str) -> Result<AdcCredentials, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read ADC credentials file: {e}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Malformed ADC credentials file: {e}"))?;
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("service_account")
    {
        "service_account" => Ok(AdcCredentials::ServiceAccount(ServiceAccountCredentials {
            client_email: required_string(&value, "client_email")?,
            private_key: required_string(&value, "private_key")?,
            token_uri: token_uri(&value),
            scopes: configured_scopes(&value, &[DEFAULT_ADC_SCOPE]),
        })),
        "authorized_user" => Ok(AdcCredentials::AuthorizedUser(AuthorizedUserCredentials {
            client_id: required_string(&value, "client_id")?,
            client_secret: required_string(&value, "client_secret")?,
            refresh_token: required_string(&value, "refresh_token")?,
            token_uri: token_uri(&value),
            scopes: configured_scopes(&value, &[]),
        })),
        kind => Err(format!("Unsupported ADC credential type: {kind}")),
    }
}

/// Build a base64url-encoded self-signed JWT for a service account
/// (RS256). Reuses the JWT claims google-auth-library sends.
pub fn build_self_signed_jwt(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    now_secs: u64,
) -> Result<String, String> {
    build_self_signed_jwt_with_scopes(
        client_email,
        private_key_pem,
        token_uri,
        &[DEFAULT_ADC_SCOPE.to_string()],
        now_secs,
    )
}

fn build_self_signed_jwt_with_scopes(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    scopes: &[String],
    now_secs: u64,
) -> Result<String, String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let scope = if scopes.is_empty() {
        DEFAULT_ADC_SCOPE.to_string()
    } else {
        scopes.join(" ")
    };
    let claims = json!({
        "iss": client_email,
        "scope": scope,
        "aud": token_uri,
        "iat": now_secs,
        "exp": now_secs.saturating_add(3600),
    });
    // Invariant: serializing json! literals cannot fail.
    #[allow(clippy::unwrap_used)]
    let (header_b64, claims_b64) = {
        (
            engine.encode(serde_json::to_vec(&header).unwrap()),
            engine.encode(serde_json::to_vec(&claims).unwrap()),
        )
    };
    let signing_input = format!("{header_b64}.{claims_b64}");

    let key_pair = decode_rsa_key(private_key_pem)?;
    let mut signature = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|e| format!("Failed to sign JWT: {e}"))?;
    Ok(format!("{signing_input}.{}", engine.encode(&signature)))
}

fn decode_rsa_key(pem: &str) -> Result<ring::signature::RsaKeyPair, String> {
    let pem_trimmed = pem.trim();
    if let Some(der) = pem_to_der(pem_trimmed, "PRIVATE KEY") {
        return ring::signature::RsaKeyPair::from_pkcs8(&der)
            .map_err(|e| format!("Failed to parse PKCS#8 private key: {e}"));
    }
    // PKCS#1 "RSA PRIVATE KEY": wrap in a minimal PKCS#8 structure.
    if let Some(der) = pem_to_der(pem_trimmed, "RSA PRIVATE KEY") {
        let alg_id: &[u8] = &[
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
            0x00,
        ];
        let mut content = vec![0x02, 0x01, 0x00];
        content.extend_from_slice(alg_id);
        content.push(0x04);
        content.extend_from_slice(&der_length(der.len()));
        content.extend_from_slice(&der);
        let mut pkcs8 = vec![0x30];
        pkcs8.extend_from_slice(&der_length(content.len()));
        pkcs8.extend_from_slice(&content);
        return ring::signature::RsaKeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| format!("Failed to parse PKCS#1 private key: {e}"));
    }
    Err("ADC private key is not a PEM PRIVATE KEY or RSA PRIVATE KEY".to_string())
}

fn der_length(length: usize) -> Vec<u8> {
    if length < 128 {
        return vec![length as u8];
    }
    let bytes = length.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    let value = &bytes[first..];
    let mut encoded = vec![0x80 | value.len() as u8];
    encoded.extend_from_slice(value);
    encoded
}

fn pem_to_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    let lines: Vec<&str> = pem.lines().map(|line| line.trim()).collect();
    let start = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start_idx = lines.iter().position(|line| *line == start)?;
    let end_idx = lines[start_idx + 1..]
        .iter()
        .position(|line| *line == end)?
        + start_idx
        + 1;
    use base64::Engine;
    let b64: String = lines[start_idx + 1..end_idx].concat();
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

fn token_error(value: &Value) -> String {
    value
        .get("error_description")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .unwrap_or("token endpoint returned an error")
        .to_string()
}

async fn post_token(
    client: &reqwest::Client,
    token_uri: &str,
    form: &[(&str, String)],
) -> Result<String, String> {
    let response = client
        .post(token_uri)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("ADC token request failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("ADC token response failed: {e}"))?;
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("Malformed ADC token response: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "ADC token exchange failed ({}): {}",
            status.as_u16(),
            token_error(&value)
        ));
    }
    value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "ADC token response missing access_token".to_string())
}

async fn exchange_jwt_for_token(
    client: &reqwest::Client,
    token_uri: &str,
    jwt: &str,
) -> Result<String, String> {
    post_token(
        client,
        token_uri,
        &[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
            ),
            ("assertion", jwt.to_string()),
        ],
    )
    .await
}

async fn refresh_authorized_user(
    client: &reqwest::Client,
    credentials: &AuthorizedUserCredentials,
) -> Result<String, String> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", credentials.client_id.clone()),
        ("client_secret", credentials.client_secret.clone()),
        ("refresh_token", credentials.refresh_token.clone()),
    ];
    if !credentials.scopes.is_empty() {
        form.push(("scope", credentials.scopes.join(" ")));
    }
    post_token(client, &credentials.token_uri, &form).await
}

/// Resolve an ADC access token from the selected credentials file.
async fn resolve_adc_access_token(
    client: &reqwest::Client,
    env: Option<&crate::types::ProviderEnv>,
) -> Result<String, String> {
    let path = find_adc_path(env)
        .ok_or_else(|| "Vertex AI ADC credentials file was not found".to_string())?;
    match read_adc_credentials(&path)? {
        AdcCredentials::ServiceAccount(credentials) => {
            let jwt = build_self_signed_jwt_with_scopes(
                &credentials.client_email,
                &credentials.private_key,
                &credentials.token_uri,
                &credentials.scopes,
                crate::types::now_ms() / 1000,
            )?;
            exchange_jwt_for_token(client, &credentials.token_uri, &jwt).await
        }
        AdcCredentials::AuthorizedUser(credentials) => {
            refresh_authorized_user(client, &credentials).await
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCz9YCbyrPvK7GM
jEN6TpELtSTg4HscjCCIsCxIgSdN7MZ8dn8wpxhM0pnSRtEDi+HRtKxk+togt1ln
PKPLNmuKK5yFGeTHG5fN9mGnOFHow3YPNMwiOZ4yrP+0rBLNgT03ZuWt2t8b8wBs
T/Rznk31eZpVxfp5OUXJZmrHMwZ8hvEeStgudxMAEoKEXJoO7XrhsEd2pWXn6tsw
GCgDn2DHbfCnr4qZE62UMFRK4b145W01V8VnPIRzOxGdXJFsGPt3xuvAQnwk5L4G
t8RxMSLv+vVoWtzhsc1cbroe9L90+LJiQP2K7y/UvT6D1YO+7Dv5LMpbqwglStSO
QwJYu0zZAgMBAAECggEAED8HI8lqchqbNkmJY/bI0GpDkIujgaHC5CQnc0o5nqbU
CnN2KxHCt1jB60JaZzwPIGvzrlAZNh/nUdMfJF7e2YPzZu6+AR2kGEN4cGy8tEtF
Er1c+nACMKf+k7R/JA9ZU/GVpZrfTnojHSQguPlfJ1yZisnLQXtiqfp1hFM+cCpl
n3Ac4BCfujn0TtBYBMIjT6VI4jz4NsWhdcVlWtgyRws5NMIgedJsSVZt/WKSGeEt
N8ziBwRfTjMT6t+uJj0KDOF09yrzpf9nTvgpLHvl+iIpoL0wUynOtW38lloe8eIr
cMLbBUPyVeyVTQ4D26qFYnm1Ph+3kIC5ddloo8g0yQKBgQDp4Vd85QfdwZk2RWtU
joyZVrHB9d2U+yPB8m7i36KdYqY3DdNg9o2RWNTPRkRXiduti8MulsNcPESa5Qoa
FWAA07KzWOxrWLBdYnjPnt4Si+V4s6AtpkSmIiBYp3igUnZTiOzJgzLh+pMoVZws
nKTVZiT+cSvpY258sQkdlf/rNQKBgQDE+qGFjYSXfgyrgyCoeXhlBk5k/Ew4hYuH
bd5xJ/zNHmdEmlfbqqmsLQw8qLAGqG7Xkcc4mc61DTagWMI6gZ2ORxKEkrH0dXLH
j94dUJ+ABezIEBlrOE4oGsE3KCR3v8lWyESoVOKiy2RAeElahc1sdYRhM2J30Q6l
Ev5KkF+rlQKBgQCUSgVfshO/ve135KH91fg9jSNd2Jcqy+VLJny6KpN/eLnstD5u
/0SZgJpF5caVPlpj+fbCRmMNy0SwdUJncWAShieK4XndQjlorHPvKEqjtcHEOxf3
ebGTKJYbv+uSs1ZE9s8zoZUUhPzjGQzRmGxGxeH01irCawH124XtFVtTdQKBgEm/
sKPJFWCG0AWTBbIuMHZagxVqJLtwvInLB+KD3zGI9Y8I3mYfIoGVKCS535XOkBlj
uhwl8e91cANe1/GBv9SaJYO/TKNDKeMvqTB+lAkhrsJEzM+I+DIpujeFbwnqo147
gwEnLudWkUVWA9jBieTWpuahj3derUX+s3iFT1x1AoGBAL9A2z8nCfpTYV/0Ls93
lwX3VeJCSr71H0kRXJP41wSs3BLaekUN46psQ5gvkLfGjbnAKnogLlvaiWYlfWNm
PMgLxH//0PXY7k2j5xPHlO+UQVcZ5waO2ySGdBfljTeFtWi1UryhV6o8N22ICPzY
pxb9Ao9R6mqLWjzEaSeYzN4o
-----END PRIVATE KEY-----
"#;
    fn vertex_model() -> Model {
        let mut m = Model::new(
            "gemini-3-flash-preview",
            "Gemini 3 Flash",
            "google-vertex",
            "google-vertex",
        );
        m.reasoning = true;
        m.base_url = "https://{location}-aiplatform.googleapis.com".to_string();
        m
    }

    async fn token_fixture(body: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 16 * 1024];
            let size = socket.read(&mut request).await.unwrap();
            request.truncate(size);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/token"), handle)
    }

    async fn retry_stream_fixture(
        body: &str,
        retry_after_ms: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = body.to_string();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, status_text, body, retry_after) in [
                (
                    503,
                    "Service Unavailable",
                    "{\"error\":{\"message\":\"temporary outage\"}}".to_string(),
                    Some(retry_after_ms),
                ),
                (200, "OK", response_body, None),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let size = socket.read(&mut request).await.unwrap();
                request.truncate(size);
                requests.push(String::from_utf8(request).unwrap());
                let retry_header = retry_after
                    .map(|value| format!("retry-after-ms: {value}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status} {status_text}\r\ncontent-type: text/event-stream\r\n{retry_header}content-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn write_adc_fixture(label: &str, value: Value) -> String {
        let path = std::env::temp_dir().join(format!(
            "pi-ai-vertex-{label}-{}-{}.json",
            std::process::id(),
            crate::types::now_ms()
        ));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn parse_form(request: &str) -> std::collections::BTreeMap<String, String> {
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn adc_path_explicit_value_wins_over_default_home() {
        assert_eq!(
            resolve_adc_path(Some("/tmp/missing-adc.json"), Some("/home/test")),
            "/tmp/missing-adc.json"
        );
        assert_eq!(
            resolve_adc_path(None, Some("/home/test")),
            "/home/test/.config/gcloud/application_default_credentials.json"
        );
    }

    #[test]
    fn api_key_request_does_not_require_project_or_location() {
        let options = GoogleVertexOptions::default();
        let (url, headers) = build_request(
            &vertex_model(),
            &options,
            None,
            None,
            Some("key"),
            None,
            API_VERSION,
        );
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        assert!(headers
            .iter()
            .any(|(name, value)| name == "x-goog-api-key" && value == "key"));
    }

    #[tokio::test]
    async fn stream_api_key_uses_publisher_path_without_project_or_location() {
        let body = r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}

"#;
        let (base_url, server) = token_fixture(body).await;
        let mut model = vertex_model();
        model.base_url = base_url;
        let stream = stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            Some("real-api-key"),
            &GoogleVertexOptions::default(),
        );
        let (_, message) = stream.collect().await;
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /token/v1/publishers/google/models/"));
        assert!(request.contains("x-goog-api-key: real-api-key"));
        assert!(!message
            .error_message()
            .unwrap_or("")
            .contains("Vertex AI requires a project ID"));
    }

    #[tokio::test]
    async fn stream_retries_retryable_vertex_response_and_replays_request() {
        let body = r#"data: {"candidates":[{"content":{"parts":[{"text":"retried"}]},"finishReason":"STOP"}]}

"#;
        let (base_url, server) = retry_stream_fixture(body, "0").await;
        let mut model = vertex_model();
        model.base_url = base_url;
        let mut options = GoogleVertexOptions::default();
        options.base.base.max_retries = Some(1);
        options.base.base.max_retry_delay_ms = Some(1);

        let (_, message) = stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            Some("retry-key"),
            &options,
        )
        .collect()
        .await;
        let requests = server.await.unwrap();

        assert_eq!(requests.len(), 2);
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert!(message
            .content()
            .iter()
            .any(|block| matches!(block, crate::types::ContentBlock::Text { text, .. } if text == "retried")));
    }

    #[tokio::test]
    async fn stream_abort_interrupts_vertex_retry_backoff() {
        let body = r#"data: {"candidates":[{"content":{"parts":[{"text":"never"}]},"finishReason":"STOP"}]}

"#;
        let (base_url, server) = retry_stream_fixture(body, "10000").await;
        let mut model = vertex_model();
        model.base_url = base_url;
        let signal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut options = GoogleVertexOptions::default();
        options.base.base.max_retries = Some(1);
        options.base.base.max_retry_delay_ms = Some(0);
        options.base.abort_signal = Some(signal.clone());

        let stream = stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            Some("retry-key"),
            &options,
        );
        let collecting = tokio::spawn(async move { stream.collect().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, message) = collecting.await.unwrap();

        assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
        server.abort();
    }

    #[tokio::test]
    async fn adc_service_account_uses_token_uri_and_configured_scopes() {
        let (token_uri, server) = token_fixture(r#"{"access_token":"service-token"}"#).await;
        let path = write_adc_fixture(
            "service-account",
            json!({
                "type": "service_account",
                "client_email": "sa@example.iam.gserviceaccount.com",
                "private_key": KEY,
                "token_uri": token_uri,
                "scopes": ["scope.one", "scope.two"]
            }),
        );
        let mut env = crate::types::ProviderEnv::new();
        env.insert("GOOGLE_APPLICATION_CREDENTIALS".to_string(), path.clone());
        let token = resolve_adc_access_token(&reqwest::Client::new(), Some(&env))
            .await
            .unwrap();
        assert_eq!(token, "service-token");
        let request = server.await.unwrap();
        let form = parse_form(&request);
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
        );
        let assertion = form.get("assertion").unwrap();
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(assertion.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["scope"], json!("scope.one scope.two"));
        assert_eq!(claims["aud"], json!(token_uri));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn adc_authorized_user_refreshes_with_file_credentials() {
        let (token_uri, server) = token_fixture(r#"{"access_token":"user-token"}"#).await;
        let path = write_adc_fixture(
            "authorized-user",
            json!({
                "type": "authorized_user",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "refresh_token": "refresh-token",
                "token_uri": token_uri,
                "scope": "scope.user.one scope.user.two"
            }),
        );
        let mut env = crate::types::ProviderEnv::new();
        env.insert("GOOGLE_APPLICATION_CREDENTIALS".to_string(), path.clone());
        let token = resolve_adc_access_token(&reqwest::Client::new(), Some(&env))
            .await
            .unwrap();
        assert_eq!(token, "user-token");
        let request = server.await.unwrap();
        let form = parse_form(&request);
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(form.get("client_id").map(String::as_str), Some("client-id"));
        assert_eq!(
            form.get("client_secret").map(String::as_str),
            Some("client-secret")
        );
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("refresh-token")
        );
        assert_eq!(
            form.get("scope").map(String::as_str),
            Some("scope.user.one scope.user.two")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn api_key_markers_fall_back_to_adc() {
        assert_eq!(resolve_api_key(Some("<authenticated>")), None);
        assert_eq!(resolve_api_key(Some("gcp-vertex-credentials")), None);
        assert_eq!(
            resolve_api_key(Some("  AIzaSyExampleKey123 ")).unwrap(),
            "AIzaSyExampleKey123"
        );
        assert_eq!(resolve_api_key(None), None);
    }

    #[test]
    fn resolve_project_and_location() {
        assert!(resolve_project(None, None).is_err());
        assert!(resolve_project(Some("test-project"), None).is_ok());
        let mut env = crate::types::ProviderEnv::new();
        env.insert("GCLOUD_PROJECT".to_string(), "from-env".to_string());
        assert_eq!(resolve_project(None, Some(&env)).unwrap(), "from-env");
        assert!(resolve_location(None, Some(&env)).is_err());
        env.insert(
            "GOOGLE_CLOUD_LOCATION".to_string(),
            "us-central1".to_string(),
        );
        assert_eq!(resolve_location(None, Some(&env)).unwrap(), "us-central1");
    }

    #[test]
    fn custom_base_url_resolution() {
        assert_eq!(
            resolve_custom_base_url("https://{location}-aiplatform.googleapis.com"),
            None
        );
        assert_eq!(resolve_custom_base_url("  "), None);
        assert_eq!(
            resolve_custom_base_url("https://proxy.example.com").unwrap(),
            "https://proxy.example.com"
        );
        assert!(base_url_includes_api_version(
            "https://proxy.example.com/v1/projects/x"
        ));
        assert!(base_url_includes_api_version(
            "https://proxy.example.com/v1beta1"
        ));
        assert!(!base_url_includes_api_version("https://proxy.example.com"));
    }

    #[test]
    fn build_request_urls_and_headers() {
        let options = GoogleVertexOptions::default();
        let (url, headers) = build_request(
            &vertex_model(),
            &options,
            Some("test-project"),
            Some("us-central1"),
            Some("key"),
            None,
            API_VERSION,
        );
        assert_eq!(
            url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-goog-api-key" && v == "key"));
        assert!(headers.iter().any(|(k, _)| k == "User-Agent"));
    }

    #[test]
    fn user_agent_matches_node_runtime_shape() {
        let expected = format!("pi ({} {}; {})", node_platform(), os_release(), node_arch());
        assert_eq!(pi_user_agent(), expected);
        assert!(!pi_user_agent().contains("pi (linux)"));
    }

    #[test]
    fn build_request_uses_custom_base_with_version_and_api_key() {
        let mut model = vertex_model();
        model.base_url =
            "https://proxy.example.com/v1/projects/test-project/locations/global".to_string();
        let options = GoogleVertexOptions::default();
        let (url, _) = build_request(
            &model,
            &options,
            Some("test-project"),
            Some("us-central1"),
            Some("key"),
            None,
            API_VERSION,
        );
        // baseUrl is the collection root: the resource path appends after it
        // and the version segment is not duplicated (SDK COLLECTION scope).
        assert_eq!(
            url,
            "https://proxy.example.com/v1/projects/test-project/locations/global/projects/test-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn build_request_lets_explicit_headers_override_user_agent() {
        let mut options = GoogleVertexOptions::default();
        options.base.base.headers = Some({
            let mut h = std::collections::BTreeMap::new();
            h.insert("User-Agent".to_string(), Some("custom-agent".to_string()));
            h
        });
        let (_, headers) = build_request(
            &vertex_model(),
            &options,
            Some("p"),
            Some("l"),
            None,
            None,
            API_VERSION,
        );
        let ua = headers
            .iter()
            .find(|(k, _)| k == "User-Agent")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(ua, "custom-agent");
        assert_eq!(headers.iter().filter(|(k, _)| k == "User-Agent").count(), 1);
    }

    #[test]
    fn build_request_null_headers_suppress_inherited_headers() {
        let mut model = vertex_model();
        model.headers = Some({
            let mut headers = std::collections::BTreeMap::new();
            headers.insert("X-Model-Header".to_string(), "model-value".to_string());
            headers
        });
        let mut options = GoogleVertexOptions::default();
        options.base.base.headers = Some({
            let mut headers = std::collections::BTreeMap::new();
            headers.insert("x-model-header".to_string(), None);
            headers.insert("user-agent".to_string(), None);
            headers
        });

        let (_, headers) = build_request(
            &model,
            &options,
            Some("project"),
            Some("location"),
            None,
            None,
            API_VERSION,
        );

        assert!(!headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-model-header")));
        assert!(!headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent")));
    }

    #[tokio::test]
    async fn stream_missing_project_surfaces_error_event() {
        let model = vertex_model();
        let client = reqwest::Client::new();
        let mut opts = GoogleVertexOptions::default();
        // No project/location anywhere -> resolution error becomes a terminal
        // error event, matching the upstream throw-inside-async-wrap behavior.
        let s = stream(
            &model,
            &Context::default(),
            client,
            Some("<authenticated>"),
            &opts,
        );
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert!(message
            .error_message()
            .unwrap_or("")
            .contains("Vertex AI requires a project ID"));
        let _ = &mut opts;
    }

    #[test]
    fn jwt_signing_produces_three_parts() {
        let jwt = build_self_signed_jwt(
            "sa@example.iam.gserviceaccount.com",
            KEY,
            "https://oauth2.googleapis.com/token",
            1_700_000_000,
        )
        .unwrap();
        assert_eq!(jwt.split('.').count(), 3);
        let parts: Vec<&str> = jwt.split('.').collect();
        use base64::Engine;
        let header: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[0])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header["alg"], json!("RS256"));
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iss"], json!("sa@example.iam.gserviceaccount.com"));
        assert_eq!(claims["aud"], json!("https://oauth2.googleapis.com/token"));
    }
}
