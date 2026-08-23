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
//! DOCUMENTED DIVERGENCE (ADC token acquisition): upstream delegates to
//! `@google/genai` + google-auth-library, which implements the full ADC chain
//! (metadata server, gcloud CLI, service account files). This port supports
//! the service-account file path used by `gcloud auth application-default
//! login`: it reads `GOOGLE_APPLICATION_CREDENTIALS`, builds a self-signed
//! JWT (RS256 with the service account private key), and exchanges it at the
//! token_uri for an access token. The seam is marked `TODO(adc)` below.
//! Ambient metadata-server and workload-identity resolution are not ported.

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

const API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const VERTEX_ADC_DEFAULT_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

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
    regex::Regex::new(r"^<[^>]+>$").unwrap().is_match(api_key)
}

/// Resolve the project id: options.project, `GOOGLE_CLOUD_PROJECT`, or
/// `GCLOUD_PROJECT` (upstream `resolveProject`).
pub fn resolve_project(
    project: Option<&str>,
    env: Option<&crate::types::ProviderEnv>,
) -> Result<String, String> {
    let project = project
        .map(|s| s.to_string())
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
    let location = location
        .map(|s| s.to_string())
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_LOCATION", env));
    location.ok_or_else(|| {
        "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
            .to_string()
    })
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
    let path_has_version = base_url.split('/').any(|part| {
        regex::Regex::new(r"^v\d+(?:beta\d*)?$")
            .unwrap()
            .is_match(part)
    });
    if path_has_version {
        return true;
    }
    regex::Regex::new(r"(?:^|/)v\d+(?:beta\d*)?(?:/|$)")
        .unwrap()
        .is_match(base_url)
}

/// Compute the URL + headers for a Vertex `:streamGenerateContent` request.
/// Returns (url, headers).
fn build_request(
    model: &Model,
    options: &GoogleVertexOptions,
    project: &str,
    location: &str,
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
        .unwrap_or_else(|| format!("https://{location}-aiplatform.googleapis.com"))
        .trim_end_matches('/')
        .to_string();
    let url = format!(
        "{base}{version_segment}/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
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
            if let Some(v) = v {
                headers.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
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
    format!("pi ({})", std::env::consts::OS)
}

fn is_gemini3_pro_model(id: &str) -> bool {
    regex::Regex::new(r"(?i)gemini-3(?:\.\d+)?-pro")
        .unwrap()
        .is_match(id)
}

fn is_gemini3_flash_model(id: &str) -> bool {
    let id = id.to_lowercase();
    regex::Regex::new(r"gemini-3(?:\.\d+)?-flash")
        .unwrap()
        .is_match(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
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

/// Stream a request against the Vertex AI endpoint.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &GoogleVertexOptions,
) -> AssistantMessageEventStream {
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

            let project =
                resolve_project(options.project.as_deref(), options.base.base.env.as_ref())?;
            let location =
                resolve_location(options.location.as_deref(), options.base.base.env.as_ref())?;

            let params = build_params(&model, &context, &to_google_options(&options));

            let bearer = if api_key.is_none() {
                resolve_adc_access_token(options.base.base.env.as_ref()).await
            } else {
                None
            };
            let (endpoint, headers) = build_request(
                &model,
                &options,
                &project,
                &location,
                api_key.as_deref(),
                bearer.as_deref(),
                API_VERSION,
            );

            let mut request = client
                .post(&endpoint)
                .header("content-type", "application/json")
                .json(&params);
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request
                .send()
                .await
                .map_err(|err| format!("Request failed: {err}"))?;
            let status = response.status();
            let provider_response = crate::types::ProviderResponse {
                status: status.as_u16(),
                headers: Default::default(),
            };
            if let Some(on_response) = &options.base.on_response {
                on_response(&provider_response, &model);
            }
            let body = response
                .bytes()
                .await
                .map_err(|err| format!("Request body failed: {err}"))?;
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

    let reasoning = options.reasoning.unwrap();
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
// Application Default Credentials (service-account file path)
// ---------------------------------------------------------------------------

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| path.to_string())
    } else {
        path.to_string()
    }
}

fn find_adc_path(env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env).or_else(|| {
        if std::path::Path::new(&expand_home(VERTEX_ADC_DEFAULT_PATH)).exists() {
            Some(expand_home(VERTEX_ADC_DEFAULT_PATH))
        } else {
            None
        }
    })
}

fn read_service_account(path: &str) -> Result<(String, String, String), String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read ADC credentials file: {e}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Malformed ADC credentials file: {e}"))?;
    let client_email = value
        .get("client_email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "ADC file missing client_email".to_string())?;
    let private_key = value
        .get("private_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "ADC file missing private_key".to_string())?;
    let token_uri = value
        .get("token_uri")
        .and_then(|v| v.as_str())
        .unwrap_or("https://oauth2.googleapis.com/token")
        .to_string();
    Ok((client_email.to_string(), private_key.to_string(), token_uri))
}

/// Build a base64url-encoded self-signed JWT for a service account
/// (RS256). Reuses the JWT claims google-auth-library sends.
pub fn build_self_signed_jwt(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    now_secs: u64,
) -> Result<String, String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": client_email,
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "aud": token_uri,
        "iat": now_secs,
        "exp": now_secs + 3600,
    });
    let header_b64 = engine.encode(serde_json::to_vec(&header).unwrap());
    let claims_b64 = engine.encode(serde_json::to_vec(&claims).unwrap());
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
        return ring::signature::RsaKeyPair::from_pkcs8(der)
            .map_err(|e| format!("Failed to parse PKCS#8 private key: {e}"));
    }
    // PKCS#1 "RSA PRIVATE KEY": wrap in a minimal PKCS#8 structure.
    if let Some(der) = pem_to_der(pem_trimmed, "RSA PRIVATE KEY") {
        // PKCS#8 = SEQUENCE { version: 0, AlgorithmIdentifier, OCTET STRING der }
        let mut pkcs8 = vec![0x30];
        let alg_id: &[u8] = &[
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
            0x00,
        ];
        let inner = der.len() + alg_id.len() + 2 + 2; // octet string header
        let len = inner + 1; // version byte content byte
        let content = {
            let mut v = vec![0x00]; // version 0
            v.extend_from_slice(alg_id);
            v.push(0x04);
            v.push(der.len() as u8);
            v.extend_from_slice(der);
            v
        };
        pkcs8.push(len as u8);
        pkcs8.extend_from_slice(&content);
        return ring::signature::RsaKeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| format!("Failed to parse PKCS#1 private key: {e}"));
    }
    Err("ADC private key is not a PEM PRIVATE KEY or RSA PRIVATE KEY".to_string())
}

fn pem_to_der<'a>(pem: &'a str, label: &str) -> Option<&'a [u8]> {
    let lines: Vec<&str> = pem.lines().map(|l| l.trim()).collect();
    let start = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start_idx = lines.iter().position(|l| *l == start)?;
    let end_idx = lines[start_idx + 1..].iter().position(|l| *l == end)? + start_idx + 1;
    use base64::Engine;
    let b64: String = lines[start_idx + 1..end_idx].concat();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .map(|v| Box::leak(v.into_boxed_slice()) as &[u8])
}

/// Resolve an ADC access token via the self-signed JWT exchange (upstream's
/// google-auth-library does this internally). `TODO(adc)`: only the
/// service-account file path is supported.
async fn resolve_adc_access_token(env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    let path = find_adc_path(env)?;
    let (client_email, private_key, token_uri) = read_service_account(&path).ok()?;
    let jwt = build_self_signed_jwt(
        &client_email,
        &private_key,
        &token_uri,
        crate::types::now_ms() / 1000,
    )
    .ok()?;
    exchange_jwt_for_token(&token_uri, &jwt).await
}

async fn exchange_jwt_for_token(token_uri: &str, jwt: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let response = client
        .post(token_uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={jwt}"
        ))
        .send()
        .await
        .ok()?;
    let body: Value = response.json().await.ok()?;
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "test-project",
            "us-central1",
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
    fn build_request_uses_custom_base_with_version_and_api_key() {
        let mut model = vertex_model();
        model.base_url =
            "https://proxy.example.com/v1/projects/test-project/locations/global".to_string();
        let options = GoogleVertexOptions::default();
        let (url, _) = build_request(
            &model,
            &options,
            "test-project",
            "us-central1",
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
        let (_, headers) =
            build_request(&vertex_model(), &options, "p", "l", None, None, API_VERSION);
        let ua = headers
            .iter()
            .find(|(k, _)| k == "User-Agent")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(ua, "custom-agent");
        assert_eq!(headers.iter().filter(|(k, _)| k == "User-Agent").count(), 1);
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
