//! Amazon Bedrock Converse stream adaptor — port of
//! `packages/ai/src/api/bedrock-converse-stream.ts`.
//!
//! Talks to the Bedrock Runtime `ConverseStream` API (`/model/{id}/converse-stream`)
//! with SigV4 request signing (hand-rolled over ring/`sha2`, wire-identical to
//! `aws-sigv4`), parses the binary `application/vnd.amazon.eventstream` response
//! frames, and emits the unified `AssistantMessageEvent` protocol.
//!
//! Auth sources (upstream order): an explicit bearer token
//! (`options.bearerToken` / `options.apiKey` / `AWS_BEARER_TOKEN_BEDROCK`)
//! bypasses SigV4 and sends `Authorization: Bearer`; otherwise ambient
//! `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ optional
//! `AWS_SESSION_TOKEN`) are signed with the resolved region/service
//! (`bedrock`). `AWS_BEDROCK_SKIP_AUTH=1` signs with dummy credentials
//! (upstream proxy mode). The AWS profile credential-chain config file path
//! (`~/.aws/credentials` + `AWS_PROFILE`) is NOT ported — documented
//! divergence; the seam is `TODO(profile)` below.
//!
//! Error diagnostics (`bedrock_response_failure` on the assistant message)
//! are not attached because the ported `AssistantMessage` type has no
//! `diagnostics` field; error message prefixes and metadata that matter for
//! retry classification are preserved verbatim.

use std::collections::BTreeMap;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::{calculate_cost, Model};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context, DoneReason,
    ErrorReason, Message, SimpleStreamOptions, StopReason, StreamOptions, Tool, ToolChoice,
    ToolResultMessage, Usage, UserContent, UserContentBody,
};

use super::transform_messages::transform_messages;

/// Matches the placeholder the Anthropic path uses for redacted thinking.
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";

/// Human-readable prefixes for Bedrock SDK exception names (upstream
/// `BEDROCK_ERROR_PREFIXES`).
fn bedrock_error_prefix(name: &str) -> &str {
    match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

/// Options for Bedrock requests (subset of upstream `BedrockOptions`).
#[derive(Clone, Default)]
pub struct BedrockOptions {
    pub base: StreamOptions,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<String>,
    pub thinking_budgets: Option<crate::types::ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    pub thinking_display: Option<String>,
    pub request_metadata: Option<BTreeMap<String, String>>,
    pub max_tokens: Option<u64>,
}

fn new_output(model: &Model) -> AssistantMessage {
    let mut output = AssistantMessage::new();
    output.set_api_provider_model(&model.api, &model.provider, &model.id);
    output.set_stop_reason(StopReason::Pending);
    let AssistantMessage::Assistant { usage, .. } = &mut output;
    *usage = Some(Usage::default());
    output
}

fn set_error_message(message: &mut AssistantMessage, text: String) {
    let AssistantMessage::Assistant { error_message, .. } = message;
    *error_message = Some(text);
}

fn get_provider_env_value(name: &str, env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    super::openai_completions::get_provider_env_value(name, env)
}

// ---------------------------------------------------------------------------
// Region / endpoint / credentials resolution
// ---------------------------------------------------------------------------

/// `getConfiguredBedrockRegion`: options.region || AWS_REGION || AWS_DEFAULT_REGION.
pub fn get_configured_bedrock_region(options: &BedrockOptions) -> Option<String> {
    options
        .region
        .clone()
        .or_else(|| get_provider_env_value("AWS_REGION", options.base.base.env.as_ref()))
        .or_else(|| get_provider_env_value("AWS_DEFAULT_REGION", options.base.base.env.as_ref()))
}

/// `getStandardBedrockEndpointRegion`: parse a `bedrock-runtime[-fips].{region}.amazonaws.com(.cn)`
/// hostname from a base URL.
pub fn get_standard_bedrock_endpoint_region(base_url: &str) -> Option<String> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()?
        .to_lowercase();
    let re = regex::Regex::new(r"^bedrock-runtime(?:-fips)?\.([a-z0-9-]+)\.amazonaws\.com(?:\.cn)?$").unwrap();
    re.captures(&host).map(|c| c[1].to_string())
}

/// `shouldUseExplicitBedrockEndpoint`: custom endpoints always pinned;
/// standard endpoints pinned only when no configured region and no ambient
/// `AWS_PROFILE`.
pub fn should_use_explicit_bedrock_endpoint(
    base_url: &str,
    configured_region: Option<&str>,
    has_ambient_profile: bool,
) -> bool {
    if get_standard_bedrock_endpoint_region(base_url).is_none() {
        return true;
    }
    configured_region.is_none() && !has_ambient_profile
}

/// Resolve access key/secret/session from scoped or ambient env (upstream
/// `getConfiguredBedrockCredentials`).
pub fn get_configured_bedrock_credentials(
    env: Option<&crate::types::ProviderEnv>,
) -> Option<(String, String, Option<String>)> {
    let access_key_id = get_provider_env_value("AWS_ACCESS_KEY_ID", env)?;
    let secret_access_key = get_provider_env_value("AWS_SECRET_ACCESS_KEY", env)?;
    let session_token = get_provider_env_value("AWS_SESSION_TOKEN", env);
    Some((access_key_id, secret_access_key, session_token))
}

/// ARN-embedded region extraction for inference profile ids (upstream
/// `arnRegionMatch`).
pub fn arn_region(model_id: &str) -> Option<String> {
    let re = regex::Regex::new(r"^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):").unwrap();
    re.captures(model_id).map(|c| c[1].to_string())
}

/// GovCloud target detection (upstream `isGovCloudBedrockTarget`).
fn is_gov_cloud_bedrock_target(model: &Model, options: &BedrockOptions) -> bool {
    let region = get_configured_bedrock_region(options);
    if region.as_deref().map(|r| r.to_lowercase().starts_with("us-gov-")).unwrap_or(false) {
        return true;
    }
    let model_id = model.id.to_lowercase();
    model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

/// Resolved Bedrock client config: region, endpoint, profile, bearer token,
/// signer credentials.
#[derive(Debug, Clone)]
pub struct BedrockResolvedConfig {
    pub region: String,
    pub endpoint: String,
    pub profile: Option<String>,
    pub bearer_token: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    /// Sign with dummy credentials (upstream `AWS_BEDROCK_SKIP_AUTH`).
    pub skip_auth: bool,
}

/// Full credential/config resolution (upstream `stream` preamble).
pub fn resolve_config(
    model: &Model,
    options: &BedrockOptions,
    api_key: Option<&str>,
) -> Result<BedrockResolvedConfig, String> {
    let env = options.base.base.env.as_ref();
    let options_profile = options
        .profile
        .clone()
        .or_else(|| env.and_then(|e| e.get("AWS_PROFILE")).cloned().filter(|v| !v.is_empty()));
    let ambient_profile = get_provider_env_value("AWS_PROFILE", env);
    let profile = options_profile.clone().or_else(|| ambient_profile.clone());

    let configured_region = get_configured_bedrock_region(options);
    let has_ambient_configured_profile = ambient_profile.is_some();
    let endpoint_region = get_standard_bedrock_endpoint_region(&model.base_url);
    let use_explicit_endpoint = should_use_explicit_bedrock_endpoint(
        &model.base_url,
        configured_region.as_deref(),
        has_ambient_configured_profile,
    );

    let arn_region = arn_region(&model.id);
    let region = if let Some(arn) = arn_region {
        arn
    } else if let Some(region) = configured_region {
        region
    } else if let (Some(endpoint_region), true) = (endpoint_region, use_explicit_endpoint) {
        endpoint_region
    } else if !has_ambient_configured_profile {
        "us-east-1".to_string()
    } else {
        // Ambient profile handles region resolution through its config file.
        // Documented divergence (TODO(profile)): we fall back to us-east-1.
        "us-east-1".to_string()
    };

    let endpoint = if use_explicit_endpoint {
        model.base_url.trim_end_matches('/').to_string()
    } else {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    };

    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    let bearer_env = get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env);
    let bearer_token = if skip_auth {
        None
    } else {
        options
            .base
            .base
            .api_key
            .clone()
            .or_else(|| api_key.map(|s| s.to_string()))
            .or_else(|| bearer_env.clone())
    };

    if skip_auth {
        return Ok(BedrockResolvedConfig {
            region,
            endpoint,
            profile,
            bearer_token: None,
            access_key: Some("dummy-access-key".to_string()),
            secret_key: Some("dummy-secret-key".to_string()),
            session_token: None,
            skip_auth: true,
        });
    }

    let (access_key, secret_key, session_token): (Option<String>, Option<String>, Option<String>) =
        if bearer_token.is_none() && profile.is_none() {
            get_configured_bedrock_credentials(env).map(|(a, s, t)| (Some(a), Some(s), t)).unwrap_or((None, None, None))
        } else {
            (None, None, None)
        };
    if let (Some(a), Some(s)) = (&access_key, &secret_key) {
        let _ = (a, s);
    }

    Ok(BedrockResolvedConfig {
        region,
        endpoint,
        profile,
        bearer_token,
        access_key,
        secret_key,
        session_token,
        skip_auth,
    })
}

// ---------------------------------------------------------------------------
// SigV4 signing
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let signing_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let tag = ring::hmac::sign(&signing_key, data);
    tag.as_ref().to_vec()
}

fn uri_encode_path(path: &str) -> String {
    // Encode each path segment (RFC 3986), preserving '/'.
    let mut out = String::new();
    for segment in path.split('/') {
        if !out.is_empty() {
            out.push('/');
        }
        for b in segment.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// Sign a request with AWS SigV4 (service `bedrock`), returning the headers
/// to attach (`x-amz-date`, `authorization`, `x-amz-security-token`).
/// `uri` is the full request path+query (query signed too when non-empty).
#[allow(clippy::too_many_arguments)]
pub fn sign_aws4_request(
    method: &str,
    uri: &str,
    query: &str,
    host: &str,
    payload: &[u8],
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
    now_ms: u64,
) -> Vec<(String, String)> {
    sign_aws4_request_with_headers(
        method, uri, query, host, payload, access_key, secret_key, session_token, region, service,
        now_ms, &[],
    )
}

/// Variant with caller-signed extra headers (e.g. `content-type`). Header
/// names are lower-cased; duplicates collapse with the last value winning
/// (mirrors the AWS SDK's `signedHeaders` collection).
#[allow(clippy::too_many_arguments)]
pub fn sign_aws4_request_with_headers(
    method: &str,
    uri: &str,
    query: &str,
    host: &str,
    payload: &[u8],
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
    now_ms: u64,
    extra_headers: &[(&str, &str)],
) -> Vec<(String, String)> {
    let amz_date = format_amz_date(now_ms);
    let date = &amz_date[..8];
    let payload_hash = sha256_hex(payload);

    let mut canonical_headers: Vec<(String, String)> = vec![
        ("host".to_string(), host.to_lowercase()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    for (name, value) in extra_headers {
        let lower = name.to_lowercase();
        canonical_headers.retain(|(k, _)| *k != lower);
        canonical_headers.push((lower, value.to_string()));
    }
    if let Some(token) = session_token {
        canonical_headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_headers: Vec<String> = canonical_headers.iter().map(|(k, _)| k.clone()).collect();
    let signed_headers_str = signed_headers.join(";");
    let canonical_headers_str = canonical_headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();

    let canonical_request = format!(
        "{method}\n{uri}\n{query}\n{canonical_headers_str}\n{signed_headers_str}\n{payload_hash}"
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_lower(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let credential = format!("{access_key}/{scope}");
    let mut headers = vec![
        ("x-amz-date".to_string(), amz_date),
        (
            "authorization".to_string(),
            format!(
                "AWS4-HMAC-SHA256 Credential={credential}, SignedHeaders={signed_headers_str}, Signature={signature}"
            ),
        ),
    ];
    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    headers
}

fn format_amz_date(now_ms: u64) -> String {
    let secs = now_ms / 1000;
    // Format as UTC YYYYMMDD'T'HHMMSS'Z' without pulling chrono.
    let days = secs / 86400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Request body building
// ---------------------------------------------------------------------------

fn sanitize_surrogates(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (0xD800..=0xDFFF).contains(&(c as u32)) {
            out.push('\u{FFFD}');
        } else {
            out.push(c);
        }
    }
    out
}

/// `normalizeToolCallId`: sanitize `[^a-zA-Z0-9_-]` -> `_`, cap at 64 chars.
fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if sanitized.len() > 64 {
        sanitized.chars().take(64).collect()
    } else {
        sanitized
    }
}

fn create_non_blank_text_block(text: &str) -> Option<Value> {
    let sanitized = sanitize_surrogates(text);
    if sanitized.trim().is_empty() {
        None
    } else {
        Some(json!({ "text": sanitized }))
    }
}

fn create_required_text_block(text: &str) -> Value {
    create_non_blank_text_block(text).unwrap_or_else(|| json!({ "text": EMPTY_TEXT_PLACEHOLDER }))
}

fn create_image_block(mime_type: &str, data: &str) -> Option<Value> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(json!({ "image": { "source": { "bytes": data }, "format": format } }))
}

/// Drop empty object keys recursively (upstream `sanitizeBedrockDocument`).
fn sanitize_bedrock_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_bedrock_document).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if k.is_empty() {
                    continue;
                }
                out.insert(k.clone(), sanitize_bedrock_document(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn convert_tool_result_content(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut result = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Image { mime_type, data } => {
                if let Some(image) = create_image_block(mime_type, data) {
                    result.push(image);
                }
            }
            ContentBlock::Text { text, .. } => {
                if let Some(text_block) = create_non_blank_text_block(text) {
                    result.push(text_block);
                }
            }
            _ => {}
        }
    }
    if result.is_empty() {
        result.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
    }
    result
}

/// `supportsPromptCaching`: Claude model (id or name) with cache support, or
/// `AWS_BEDROCK_FORCE_CACHE=1`.
fn supports_prompt_caching(model: &Model, env: Option<&crate::types::ProviderEnv>) -> bool {
    let candidates = model_match_candidates(&model.id, Some(&model.name));
    let has_claude = candidates.iter().any(|s| s.contains("claude"));
    if !has_claude {
        return get_provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    if candidates.iter().any(|s| s.contains("fable-5") || s.contains("opus-5") || s.contains("sonnet-5")) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("-4-")) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("claude-3-7-sonnet")) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("claude-3-5-haiku")) {
        return true;
    }
    false
}

fn model_match_candidates(model_id: &str, model_name: Option<&str>) -> Vec<String> {
    let values: Vec<String> = match model_name {
        Some(name) => vec![model_id.to_string(), name.to_string()],
        None => vec![model_id.to_string()],
    };
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let normalized = lower.replace([' ', '_', '.', ':'], "-");
            vec![lower, normalized]
        })
        .collect()
}

fn supports_adaptive_thinking(model: &Model) -> bool {
    model_match_candidates(&model.id, Some(&model.name)).iter().any(|s| {
        s.contains("opus-4-6")
            || s.contains("opus-4-7")
            || s.contains("opus-4-8")
            || s.contains("opus-5")
            || s.contains("sonnet-4-6")
            || s.contains("sonnet-5")
            || s.contains("fable-5")
    })
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    model_match_candidates(&model.id, Some(&model.name)).iter().any(|s| {
        s.contains("opus-4-7")
            || s.contains("opus-4-8")
            || s.contains("opus-5")
            || s.contains("sonnet-5")
            || s.contains("fable-5")
    })
}

fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn supports_thinking_signature(model: &Model) -> bool {
    is_anthropic_claude_model(model)
}

/// `resolveCacheRetention`: default "short", `PI_CACHE_RETENTION=long` maps
/// to "long".
fn resolve_cache_retention(cache_retention: Option<&CacheRetention>, env: Option<&crate::types::ProviderEnv>) -> String {
    if let Some(retention) = cache_retention {
        return retention.clone();
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return "long".to_string();
    }
    "short".to_string()
}

/// `buildSystemPrompt`: text block plus an optional cache point for
/// supported Claude models.
fn build_system_prompt(system_prompt: Option<&str>, model: &Model, cache_retention: &str, env: Option<&crate::types::ProviderEnv>) -> Option<Value> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": sanitize_surrogates(system_prompt) })];
    if cache_retention != "none" && supports_prompt_caching(model, env) {
        let mut cache_point = json!({ "type": "default" });
        if cache_retention == "long" {
            cache_point["ttl"] = json!("ONE_HOUR");
        }
        blocks.push(json!({ "cachePoint": cache_point }));
    }
    Some(Value::Array(blocks))
}

/// Decode a stored base64 redacted payload (upstream `decodeRedactedContent`).
fn decode_redacted_content(signature: Option<&str>) -> Option<Vec<u8>> {
    let signature = signature?;
    base64::engine::general_purpose::STANDARD.decode(signature).ok()
}

/// Encode redacted chunks to one base64 signature (upstream `bytesToBase64`).
fn bytes_to_base64(chunks: &[Vec<u8>]) -> String {
    let mut all = Vec::new();
    for chunk in chunks {
        all.extend_from_slice(chunk);
    }
    base64::engine::general_purpose::STANDARD.encode(&all)
}

/// `convertMessages` — the message array for ConverseStream.
pub fn convert_messages(
    context: &Context,
    model: &Model,
    cache_retention: &str,
    env: Option<&crate::types::ProviderEnv>,
) -> Vec<Value> {
    let transformed = transform_messages(&context.messages, model, Some(&|id: &str, _m: &Model, _s: &AssistantMessage| {
        normalize_tool_call_id(id)
    }));

    let mut result: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(UserContent::RoleUser { content, .. }) => {
                let mut blocks = Vec::new();
                match content {
                    UserContentBody::String(s) => {
                        blocks.push(create_required_text_block(s));
                    }
                    UserContentBody::Blocks(blocks_in) => {
                        for block in blocks_in {
                            match block {
                                ContentBlock::Text { text, .. } => {
                                    if let Some(text_block) = create_non_blank_text_block(text) {
                                        blocks.push(text_block);
                                    }
                                }
                                ContentBlock::Image { mime_type, data } => {
                                    if let Some(image) = create_image_block(mime_type, data) {
                                        blocks.push(image);
                                    }
                                }
                                _ => continue,
                            }
                        }
                        if blocks.is_empty() {
                            blocks.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": blocks }));
            }
            Message::Assistant(assistant) => {
                if assistant.content().is_empty() {
                    // Bedrock rejects messages with empty content arrays.
                    i += 1;
                    continue;
                }
                let mut blocks = Vec::new();
                for block in assistant.content() {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if let Some(text_block) = create_non_blank_text_block(text) {
                                blocks.push(text_block);
                            }
                        }
                        ContentBlock::ToolCall { id, name, arguments, .. } => {
                            blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": id,
                                    "name": name,
                                    "input": sanitize_bedrock_document(arguments),
                                }
                            }));
                        }
                        ContentBlock::Thinking { thinking, thinking_signature, redacted, .. } => {
                            if *redacted == Some(true) {
                                let redacted_content = decode_redacted_content(thinking_signature.as_deref());
                                if let Some(redacted_content) = redacted_content {
                                    if !redacted_content.is_empty() {
                                        blocks.push(json!({
                                            "reasoningContent": {
                                                "redactedContent": base64::engine::general_purpose::STANDARD.encode(&redacted_content)
                                            }
                                        }));
                                    }
                                }
                                continue;
                            }
                            let thinking = sanitize_surrogates(thinking);
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            if supports_thinking_signature(model) {
                                if thinking_signature.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                                    blocks.push(json!({
                                        "reasoningContent": {
                                            "reasoningText": {
                                                "text": thinking,
                                                "signature": thinking_signature.clone().unwrap_or_default(),
                                            }
                                        }
                                    }));
                                } else {
                                    blocks.push(json!({ "text": thinking }));
                                }
                            } else {
                                blocks.push(json!({
                                    "reasoningContent": { "reasoningText": { "text": thinking } }
                                }));
                            }
                        }
                        _ => continue,
                    }
                }
                if blocks.is_empty() {
                    i += 1;
                    continue;
                }
                result.push(json!({ "role": "assistant", "content": blocks }));
            }
            Message::ToolResult(tool_result) => {
                // Collect all consecutive tool results into one user message.
                let mut tool_results: Vec<Value> = Vec::new();
                tool_results.push(build_tool_result_block(tool_result));
                let mut j = i + 1;
                while j < transformed.len() {
                    if let Message::ToolResult(next) = &transformed[j] {
                        tool_results.push(build_tool_result_block(next));
                        j += 1;
                    } else {
                        break;
                    }
                }
                i = j - 1;
                result.push(json!({ "role": "user", "content": tool_results }));
            }
        }
        i += 1;
    }

    // Add cache point to the last user message for supported Claude models.
    if cache_retention != "none" && supports_prompt_caching(model, env) && !result.is_empty() {
        if let Some(last) = result.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some("user") {
                let mut cache_point = json!({ "type": "default" });
                if cache_retention == "long" {
                    cache_point["ttl"] = json!("ONE_HOUR");
                }
                if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                    content.push(json!({ "cachePoint": cache_point }));
                }
            }
        }
    }

    result
}

fn build_tool_result_block(tool_result: &ToolResultMessage) -> Value {
    let content = convert_tool_result_content(tool_result.content());
    json!({
        "toolResult": {
            "toolUseId": tool_result.tool_call_id(),
            "content": content,
            "status": if tool_result.is_error() { "error" } else { "success" },
        }
    })
}

fn resolve_json_schema_strict_sampling(tool: &Tool) -> bool {
    matches!(
        tool.constrained_sampling,
        Some(crate::types::ConstrainedSampling::JsonSchema { strict: crate::types::StrictPreference::Require })
    )
}

fn get_json_schema_tool_parameters(tool: &Tool, strict: bool) -> Value {
    let mut params = tool.parameters.clone();
    if strict {
        if let Some(obj) = params.as_object_mut() {
            // Upstream resolves `additionalProperties: false` + required for
            // strict mode through TypeBox; keey the parameters as-authored and
            // mark strict in the toolSpec wrapper.
            let _ = obj;
        }
    }
    params
}

fn compat_supports_strict_mode(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|c| c.get("supportsStrictMode"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// `convertToolConfig` — tool spec + toolChoice.
pub fn convert_tool_config(tools: &[Tool], tool_choice: Option<&Value>, supports_strict_mode: bool) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    if tool_choice.and_then(|v| v.as_str()) == Some("none") {
        return None;
    }
    let bedrock_tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let strict = supports_strict_mode && resolve_json_schema_strict_sampling(tool);
            let mut spec = json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": get_json_schema_tool_parameters(tool, strict) },
                }
            });
            if strict {
                spec["toolSpec"]["strict"] = json!(true);
            }
            spec
        })
        .collect();

    let mut bedrock_tool_choice: Option<Value> = None;
    match tool_choice {
        Some(Value::String(s)) if s == "auto" => bedrock_tool_choice = Some(json!({ "auto": {} })),
        Some(Value::String(s)) if s == "any" => bedrock_tool_choice = Some(json!({ "any": {} })),
        Some(Value::Object(map)) => {
            if let Some(name) = map.get("name").and_then(|v| v.as_str()) {
                bedrock_tool_choice = Some(json!({ "tool": { "name": name } }));
            }
        }
        _ => {}
    }

    let mut config = serde_json::Map::new();
    config.insert("tools".to_string(), Value::Array(bedrock_tools));
    if let Some(choice) = bedrock_tool_choice {
        config.insert("toolChoice".to_string(), choice);
    }
    Some(Value::Object(config))
}

/// `mapThinkingLevelToEffort` (Claude adaptive output_config.effort).
fn map_thinking_level_to_effort(model: &Model, level: Option<&str>) -> String {
    let level = level.unwrap_or("high");
    if level == "xhigh" && supports_native_xhigh_effort(model) {
        return "xhigh".to_string();
    }
    let level_enum = crate::types::ModelThinkingLevel::from_effort_str(level);
    if let Some(mapped) = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&level_enum))
        .cloned()
        .flatten()
    {
        return mapped;
    }
    match level {
        "minimal" | "low" => "low".to_string(),
        "medium" => "medium".to_string(),
        _ => "high".to_string(),
    }
}

/// `buildAdditionalModelRequestFields` — reasoning config for Claude models.
pub fn build_additional_model_request_fields(
    model: &Model,
    options: &BedrockOptions,
) -> Option<Value> {
    if options.reasoning.is_none() || !model.reasoning {
        return None;
    }
    if !is_anthropic_claude_model(model) {
        return None;
    }
    let gov = is_gov_cloud_bedrock_target(model, options);
    let display = if gov { None } else { Some(options.thinking_display.clone().unwrap_or_else(|| "summarized".to_string())) };

    let mut result = serde_json::Map::new();
    if supports_adaptive_thinking(model) {
        let mut thinking = serde_json::Map::new();
        thinking.insert("type".to_string(), json!("adaptive"));
        if let Some(display) = &display {
            thinking.insert("display".to_string(), json!(display));
        }
        result.insert("thinking".to_string(), Value::Object(thinking));
        result.insert("output_config".to_string(), json!({ "effort": map_thinking_level_to_effort(model, options.reasoning.as_deref()) }));
    } else {
        let level = options.reasoning.as_deref();
        let budget_level = if level == Some("xhigh") || level == Some("max") { "high" } else { level.unwrap_or("high") };
        let budget = options
            .thinking_budgets
            .as_ref()
            .and_then(|b| match budget_level {
                "minimal" => b.minimal,
                "low" => b.low,
                "medium" => b.medium,
                "high" => b.high,
                _ => None,
            })
            .unwrap_or(match level {
                Some("minimal") => 1024,
                Some("low") => 2048,
                Some("medium") => 8192,
                _ => 16384,
            });
        let mut thinking = serde_json::Map::new();
        thinking.insert("type".to_string(), json!("enabled"));
        thinking.insert("budget_tokens".to_string(), json!(budget));
        if let Some(display) = &display {
            thinking.insert("display".to_string(), json!(display));
        }
        result.insert("thinking".to_string(), Value::Object(thinking));
        if options.interleaved_thinking.unwrap_or(true) {
            result.insert("anthropic_beta".to_string(), json!(["interleaved-thinking-2025-05-14"]));
        }
    }
    Some(Value::Object(result))
}

/// `mapStopReason` — Bedrock stop reason to unified stop reason.
pub fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn") | Some("stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens") | Some("model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(other) => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
        None => (StopReason::Error, None),
    }
}

/// Build the ConverseStream request body (upstream `commandInput` without the
/// SDK wrapper).
pub fn build_command_input(
    model: &Model,
    context: &Context,
    options: &BedrockOptions,
) -> Value {
    let env = options.base.base.env.as_ref();
    let cache_retention = resolve_cache_retention(options.base.cache_retention.as_ref(), env);
    let inference_max_tokens = options
        .max_tokens
        .or(options.base.max_tokens)
        .or_else(|| {
            if is_anthropic_claude_model(model) {
                Some(model.max_tokens)
            } else {
                None
            }
        });

    let mut inference = serde_json::Map::new();
    if let Some(max_tokens) = inference_max_tokens {
        inference.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = options.base.temperature {
        inference.insert("temperature".to_string(), json!(temperature));
    }

    let mut body = serde_json::Map::new();
    body.insert("modelId".to_string(), json!(model.id));
    body.insert("messages".to_string(), json!(convert_messages(context, model, &cache_retention, env)));
    if let Some(system) = build_system_prompt(context.system_prompt.as_deref(), model, &cache_retention, env) {
        body.insert("system".to_string(), system);
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".to_string(), Value::Object(inference));
    }
    if let Some(tool_config) = convert_tool_config(&context.tools, options.tool_choice.as_ref(), compat_supports_strict_mode(model)) {
        body.insert("toolConfig".to_string(), tool_config);
    }
    if let Some(fields) = build_additional_model_request_fields(model, options) {
        body.insert("additionalModelRequestFields".to_string(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        body.insert("requestMetadata".to_string(), json!(metadata));
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// aws-eventstream response parsing
// ---------------------------------------------------------------------------

/// One parsed `application/vnd.amazon.eventstream` frame.
#[derive(Debug, Clone)]
pub struct EventStreamFrame {
    pub event_type: String,
    pub message_type: String,
    /// JSON payload (ConverseStream events are JSON-encoded).
    pub payload: Option<Value>,
}

/// Incremental binary eventstream decoder. Rejects truncated buffers.
pub fn decode_eventstream_frames(bytes: &[u8]) -> Result<Vec<EventStreamFrame>, String> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset + 16 <= bytes.len() {
        if bytes[offset..offset + 4] != [0x00, 0xC0, 0xDE, 0x00] {
            return Err("Invalid eventstream magic".to_string());
        }
        let total_length = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let headers_length = u32::from_be_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let prelude_crc = u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
        let expected_crc = crc32_ieee(&bytes[offset..offset + 12]);
        if prelude_crc != expected_crc {
            return Err("Eventstream prelude CRC mismatch".to_string());
        }
        if total_length < 16 || offset + total_length > bytes.len() {
            return Err(format!("Eventstream frame length {total_length} exceeds buffer"));
        }
        let message_crc_offset = offset + total_length - 4;
        let message_crc = u32::from_be_bytes(bytes[message_crc_offset..message_crc_offset + 4].try_into().unwrap());
        let expected_message_crc = crc32_ieee(&bytes[offset..message_crc_offset]);
        if message_crc != expected_message_crc {
            return Err("Eventstream message CRC mismatch".to_string());
        }

        let headers_start = offset + 16;
        let headers_end = headers_start + headers_length;
        let payload_start = headers_end;
        let payload_end = message_crc_offset;
        let headers_bytes = &bytes[headers_start..headers_end];
        let payload_bytes = &bytes[payload_start..payload_end];

        let headers = parse_event_headers(headers_bytes)?;
        let event_type = headers.get(":event-type").cloned().unwrap_or_default();
        let message_type = headers.get(":message-type").cloned().unwrap_or_default();
        let payload = if payload_bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(payload_bytes).map_err(|e| format!("Eventstream payload JSON: {e}"))?)
        };
        frames.push(EventStreamFrame { event_type, message_type, payload });
        offset += total_length;
    }
    if offset != bytes.len() {
        return Err("Trailing bytes after eventstream frames".to_string());
    }
    Ok(frames)
}

fn parse_event_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let name_len = bytes[i] as usize;
        i += 1;
        if i + name_len > bytes.len() {
            return Err("Truncated header name".to_string());
        }
        let name = String::from_utf8_lossy(&bytes[i..i + name_len]).to_string();
        i += name_len;
        if i >= bytes.len() {
            return Err("Truncated header value type".to_string());
        }
        let value_type = bytes[i];
        i += 1;
        let value = match value_type {
            0 => { // bool
                if i >= bytes.len() { return Err("Truncated bool header".to_string()); }
                let v = if bytes[i] != 0 { "true".to_string() } else { "false".to_string() };
                i += 1;
                v
            }
            1 => { // byte
                if i >= bytes.len() { return Err("Truncated byte header".to_string()); }
                let v = bytes[i].to_string();
                i += 1;
                v
            }
            2 => { // short
                if i + 2 > bytes.len() { return Err("Truncated short header".to_string()); }
                let v = i16::from_be_bytes(bytes[i..i + 2].try_into().unwrap()).to_string();
                i += 2;
                v
            }
            3 => { // int
                if i + 4 > bytes.len() { return Err("Truncated int header".to_string()); }
                let v = i32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()).to_string();
                i += 4;
                v
            }
            4 => { // long
                if i + 8 > bytes.len() { return Err("Truncated long header".to_string()); }
                let v = i64::from_be_bytes(bytes[i..i + 8].try_into().unwrap()).to_string();
                i += 8;
                v
            }
            5 | 6 | 8 => { // byte_array / string / uuid (byte_array len + bytes)
                if i + 2 > bytes.len() { return Err("Truncated byte array length".to_string()); }
                let len = u16::from_be_bytes(bytes[i..i + 2].try_into().unwrap()) as usize;
                i += 2;
                if i + len > bytes.len() { return Err("Truncated byte array value".to_string()); }
                let raw = &bytes[i..i + len];
                i += len;
                if value_type == 6 {
                    String::from_utf8_lossy(raw).to_string()
                } else {
                    base64::engine::general_purpose::STANDARD.encode(raw)
                }
            }
            7 => { // timestamp (int64 ms)
                if i + 8 > bytes.len() { return Err("Truncated timestamp header".to_string()); }
                let v = i64::from_be_bytes(bytes[i..i + 8].try_into().unwrap()).to_string();
                i += 8;
                v
            }
            other => return Err(format!("Unknown header value type {other}")),
        };
        map.insert(name, value);
    }
    Ok(map)
}

fn crc32_ieee(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

// ---------------------------------------------------------------------------
// Streaming event handling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    ToolCall,
}

#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    index: usize,
    text: String,
    thinking: String,
    thinking_signature: String,
    redacted: bool,
    redacted_chunks: Vec<Vec<u8>>,
    tool_id: String,
    tool_name: String,
    partial_json: String,
    arguments: Value,
}

impl Block {
    fn text(index: usize) -> Self {
        Self { kind: BlockKind::Text, index, text: String::new(), thinking: String::new(), thinking_signature: String::new(), redacted: false, redacted_chunks: Vec::new(), tool_id: String::new(), tool_name: String::new(), partial_json: String::new(), arguments: json!({}) }
    }
    fn thinking(index: usize) -> Self {
        Self { kind: BlockKind::Thinking, index, text: String::new(), thinking: String::new(), thinking_signature: String::new(), redacted: false, redacted_chunks: Vec::new(), tool_id: String::new(), tool_name: String::new(), partial_json: String::new(), arguments: json!({}) }
    }
    fn tool_call(index: usize, id: String, name: String) -> Self {
        Self { kind: BlockKind::ToolCall, index, text: String::new(), thinking: String::new(), thinking_signature: String::new(), redacted: false, redacted_chunks: Vec::new(), tool_id: id, tool_name: name, partial_json: String::new(), arguments: json!({}) }
    }
}

struct BedrockStreamState {
    output: AssistantMessage,
    blocks: Vec<Block>,
}

fn block_to_content(block: &Block) -> ContentBlock {
    match &block.kind {
        BlockKind::Text => ContentBlock::text(&block.text),
        BlockKind::Thinking => ContentBlock::Thinking {
            thinking: block.thinking.clone(),
            thinking_signature: if block.thinking_signature.is_empty() { None } else { Some(block.thinking_signature.clone()) },
            redacted: if block.redacted { Some(true) } else { None },
        },
        BlockKind::ToolCall => ContentBlock::tool_call(block.tool_id.clone(), block.tool_name.clone(), block.arguments.clone()),
    }
}

fn process_stream_event(
    model: &Model,
    event: &Value,
    state: &mut BedrockStreamState,
    push: &mut dyn FnMut(AssistantMessageEvent),
) -> Result<(), String> {
    if let Some(start) = event.get("messageStart") {
        if start.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            return Err("Unexpected assistant message start but got user message start instead".to_string());
        }
        push(AssistantMessageEvent::Start { partial: state.output.clone() });
    } else if let Some(block_start) = event.get("contentBlockStart") {
        let index = block_start.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let start = block_start.get("start");
        if let Some(tool_use) = start.and_then(|s| s.get("toolUse")) {
            let block = Block::tool_call(
                index,
                tool_use.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                tool_use.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            );
            let content_index = state.blocks.len();
            state.blocks.push(block);
            push(AssistantMessageEvent::ToolCallStart {
                content_index,
                partial: state.output.clone(),
            });
        }
    } else if let Some(delta_event) = event.get("contentBlockDelta") {
        let index = delta_event.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let delta = delta_event.get("delta");
        let mut block_index = state.blocks.iter().position(|b| b.index == index);
        let mut created = false;
        if block_index.is_none() {
            // Creating a new block when no start event was seen (text /
            // reasoning arrive without contentBlockStart).
            let delta_text = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()).is_some();
            let delta_reasoning = delta.and_then(|d| d.get("reasoningContent")).is_some();
            let block = if delta_text {
                Some(Block::text(index))
            } else if delta_reasoning {
                Some(Block::thinking(index))
            } else {
                None
            };
            if let Some(block) = block {
                let content_index = state.blocks.len();
                state.blocks.push(block);
                block_index = Some(content_index);
                created = true;
            }
        }
        let Some(block_index) = block_index else { return Ok(()) };
        let kind = state.blocks[block_index].kind.clone();
        if let Some(text) = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
            if created {
                push(AssistantMessageEvent::TextStart {
                    content_index: block_index,
                    partial: state.output.clone(),
                });
            }
            state.blocks[block_index].text.push_str(text);
            push(AssistantMessageEvent::TextDelta {
                content_index: block_index,
                delta: text.to_string(),
                partial: state.output.clone(),
            });
        } else if let Some(tool_use_delta) = delta.and_then(|d| d.get("toolUse")) {
            if kind == BlockKind::ToolCall {
                let input = tool_use_delta.get("input").and_then(|v| v.as_str()).unwrap_or("");
                state.blocks[block_index].partial_json.push_str(input);
                state.blocks[block_index].arguments = crate::partial_json::parse_streaming_json(&state.blocks[block_index].partial_json);
                push(AssistantMessageEvent::ToolCallDelta {
                    content_index: block_index,
                    delta: input.to_string(),
                    partial: state.output.clone(),
                });
            }
        } else if let Some(reasoning) = delta.and_then(|d| d.get("reasoningContent")) {
            if kind != BlockKind::Thinking {
                return Ok(());
            }
            if let Some(text) = reasoning.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    state.blocks[block_index].thinking.push_str(text);
                    push(AssistantMessageEvent::ThinkingDelta {
                        content_index: block_index,
                        delta: text.to_string(),
                        partial: state.output.clone(),
                    });
                }
            }
            if reasoning.get("signature").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) && !state.blocks[block_index].redacted {
                let signature = reasoning.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                state.blocks[block_index].thinking_signature.push_str(signature);
            }
            if let Some(redacted) = reasoning.get("redactedContent") {
                let bytes_value = redacted;
                if !state.blocks[block_index].redacted {
                    state.blocks[block_index].redacted = true;
                    state.blocks[block_index].thinking_signature.clear();
                    state.blocks[block_index].thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                    push(AssistantMessageEvent::ThinkingDelta {
                        content_index: block_index,
                        delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
                        partial: state.output.clone(),
                    });
                }
                let chunk = redacted_bytes(bytes_value);
                state.blocks[block_index].redacted_chunks.extend(chunk);
            }
        }
    } else if let Some(block_stop) = event.get("contentBlockStop") {
        let index = block_stop.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let Some(block_index) = state.blocks.iter().position(|b| b.index == index) else { return Ok(()) };
        let block = state.blocks[block_index].clone();
        match block.kind {
            BlockKind::Text => {
                push(AssistantMessageEvent::TextEnd {
                    content_index: block_index,
                    content: block.text.clone(),
                    partial: state.output.clone(),
                });
            }
            BlockKind::Thinking => {
                let mut finalized = block.clone();
                if finalized.redacted && !finalized.redacted_chunks.is_empty() {
                    finalized.thinking_signature = bytes_to_base64(&finalized.redacted_chunks);
                }
                finalized.redacted_chunks.clear();
                state.blocks[block_index] = finalized;
                push(AssistantMessageEvent::ThinkingEnd {
                    content_index: block_index,
                    content: block.thinking.clone(),
                    partial: state.output.clone(),
                });
            }
            BlockKind::ToolCall => {
                let finalized = {
                    let b = &mut state.blocks[block_index];
                    b.arguments = crate::partial_json::parse_streaming_json(&b.partial_json);
                    b.partial_json.clear();
                    b.clone()
                };
                push(AssistantMessageEvent::ToolCallEnd {
                    content_index: block_index,
                    tool_call: block_to_content(&finalized),
                    partial: state.output.clone(),
                });
            }
        }
    } else if let Some(message_stop) = event.get("messageStop") {
        let raw = message_stop.get("stopReason").and_then(|v| v.as_str()).unwrap_or("").to_string();
        state.output.set_raw_stop_reason(raw.clone());
        let (stop_reason, error_message) = map_stop_reason(Some(&raw));
        state.output.set_stop_reason(stop_reason);
        if let Some(error_message) = error_message {
            set_error_message(&mut state.output, error_message);
        }
    } else if let Some(metadata) = event.get("metadata") {
        if let Some(usage) = metadata.get("usage") {
            apply_usage(model, &mut state.output, usage);
        }
    } else if event.get("internalServerException").is_some()
        || event.get("modelStreamErrorException").is_some()
        || event.get("validationException").is_some()
        || event.get("throttlingException").is_some()
        || event.get("serviceUnavailableException").is_some()
    {
        let name = if event.get("internalServerException").is_some() {
            "InternalServerException"
        } else if event.get("modelStreamErrorException").is_some() {
            "ModelStreamErrorException"
        } else if event.get("validationException").is_some() {
            "ValidationException"
        } else if event.get("throttlingException").is_some() {
            "ThrottlingException"
        } else {
            "ServiceUnavailableException"
        };
        let message = exception_message(event, name);
        return Err(format!("{}: {message}", bedrock_error_prefix(name)));
    }
    Ok(())
}

/// Decode a `redactedContent` JSON member into bytes. The wire carries base64
/// (blob) strings; a length+bytes array (as some SDKs produce) is tolerated.
fn redacted_bytes(value: &Value) -> Vec<Vec<u8>> {
    match value {
        Value::String(s) => base64::engine::general_purpose::STANDARD.decode(s).map(|v| vec![v]).unwrap_or_default(),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(n) = item.as_u64() {
                    out.push(vec![n as u8]);
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

fn exception_message(event: &Value, name: &str) -> String {
    event
        .get(name)
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn apply_usage(model: &Model, output: &mut AssistantMessage, usage: &Value) {
    let mut u = Usage {
        input: usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output: usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read: usage.get("cacheReadInputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_write: usage.get("cacheWriteInputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_write_1h: None,
        reasoning: None,
        total_tokens: usage.get("totalTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cost: Default::default(),
    };
    if u.total_tokens == 0 {
        u.total_tokens = u.input + u.output;
    }
    let cost = calculate_cost(model, &u);
    u.cost = cost;
    output.set_usage(u);
}

/// Finalize all streamed blocks into the output content (runs at terminal
/// paths; upstream mirrors `finalizeStreamingBlock`).
fn finalize_blocks(state: &mut BedrockStreamState) {
    let AssistantMessage::Assistant { content, .. } = &mut state.output;
    *content = state.blocks
        .iter()
        .map(|b| {
            let mut b = b.clone();
            if b.redacted && !b.redacted_chunks.is_empty() {
                b.thinking_signature = bytes_to_base64(&b.redacted_chunks);
            }
            b.redacted_chunks.clear();
            block_to_content(&b)
        })
        .collect();
}

/// `streamSimple`: thinking budget adjustment (upstream `adjustMaxTokensForThinking`).
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: &str,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> (u64, u64) {
    let level = if reasoning_level == "xhigh" || reasoning_level == "max" { "high" } else { reasoning_level };
    let thinking_budget = match custom_budgets {
        Some(b) => match level {
            "minimal" => b.minimal,
            "low" => b.low,
            "medium" => b.medium,
            "high" => b.high,
            _ => None,
        },
        None => None,
    }
    .unwrap_or(match level {
        "minimal" => 1024,
        "low" => 2048,
        "medium" => 8192,
        _ => 16384,
    });
    let max_tokens = match base_max_tokens {
        Some(base) => base.saturating_add(thinking_budget).min(model_max_tokens),
        None => model_max_tokens,
    };
    let thinking_budget = if max_tokens <= thinking_budget {
        thinking_budget.min(max_tokens.saturating_sub(1024))
    } else {
        thinking_budget
    };
    (max_tokens, thinking_budget)
}

// ---------------------------------------------------------------------------
// stream / streamSimple
// ---------------------------------------------------------------------------

/// Stream a request against the Bedrock ConverseStream API.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &BedrockOptions,
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
        let result = run_bedrock_stream(
            &model,
            &context,
            client,
            api_key.as_deref(),
            &options,
            &mut |event: AssistantMessageEvent| pusher.push(event),
        )
        .await;
        match result {
            Ok(message) => {
                let reason = match message.stop_reason().unwrap_or(StopReason::Stop) {
                    StopReason::Stop => DoneReason::Stop,
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                pusher.push(AssistantMessageEvent::Done { reason, message: message.clone() });
                pusher.end(Some(message));
            }
            Err(err) => {
                let mut message = new_output(&model);
                message.set_stop_reason(StopReason::Error);
                set_error_message(&mut message, err);
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

async fn run_bedrock_stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &BedrockOptions,
    push: &mut (dyn FnMut(AssistantMessageEvent) + Send),
) -> Result<AssistantMessage, String> {
    let config = resolve_config(model, options, api_key)?;
    let body = build_command_input(model, context, options);
    let body_bytes = serde_json::to_vec(&body).map_err(|e| format!("Failed to serialize request: {e}"))?;

    let uri = format!("/model/{}:converse-stream", uri_encode_path(&model.id));
    let host = url_host(&config.endpoint)?;
    let url = format!("{}/model/{}:converse-stream", config.endpoint.trim_end_matches('/'), uri_encode_path(&model.id));

    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/vnd.amazon.eventstream")
        .body(body_bytes.clone());

    // Caller headers (upstream middleware semantics): skip reserved
    // SigV4/auth headers case-insensitively.
    let mut reserved_skip = Vec::new();
    if let Some(headers) = &options.base.base.headers {
        for (name, value) in headers {
            let lower = name.to_lowercase();
            if lower.starts_with("x-amz-") || lower == "authorization" || lower == "host" {
                reserved_skip.push(lower);
                continue;
            }
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
    }

    if let Some(bearer) = &config.bearer_token {
        request = request.header("authorization", format!("Bearer {bearer}"));
    } else {
        let access_key = config
            .access_key
            .clone()
            .ok_or_else(|| "Could not load credentials from any providers".to_string())?;
        let secret_key = config
            .secret_key
            .clone()
            .ok_or_else(|| "Could not load credentials from any providers".to_string())?;
        let signed_headers = sign_aws4_request_with_headers(
            "POST",
            &uri,
            "",
            &host,
            &body_bytes,
            &access_key,
            &secret_key,
            config.session_token.as_deref(),
            &config.region,
            "bedrock",
            crate::types::now_ms(),
            &[("content-type", "application/json")],
        );
        for (name, value) in signed_headers {
            request = request.header(name.as_str(), value.as_str());
        }
    }

    let response = request.send().await.map_err(|err| format!("Request failed: {err}"))?;
    let status = response.status();
    let headers_map = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(on_response) = &options.base.on_response {
        on_response(
            &crate::types::ProviderResponse { status: status.as_u16(), headers: headers_map },
            model,
        );
    }
    let response_bytes = response.bytes().await.map_err(|err| format!("Request body failed: {err}"))?;

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&response_bytes).to_string();
        return Err(format_bedrock_error(status.as_u16(), &body_text));
    }

    let frames = decode_eventstream_frames(&response_bytes)?;
    let mut state = BedrockStreamState {
        output: new_output(model),
        blocks: Vec::new(),
    };
    for frame in frames {
        let Some(payload) = &frame.payload else { continue };
        process_stream_event(model, payload, &mut state, push)?;
    }
    finalize_blocks(&mut state);
    if state.output.stop_reason() == Some(StopReason::Pending) {
        return Err("Bedrock stream ended without a stop reason".to_string());
    }
    if matches!(state.output.stop_reason(), Some(StopReason::Error) | Some(StopReason::Aborted)) {
        let message = state
            .output
            .error_message()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "An unknown error occurred".to_string());
        return Err(message);
    }
    Ok(state.output)
}

fn url_host(endpoint: &str) -> Result<String, String> {
    let rest = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    rest.split('/').next().map(|s| s.to_string()).ok_or_else(|| "Invalid endpoint".to_string())
}

/// `formatBedrockError` for non-2xx responses (no SDK exception name): status
/// + body with the data-retention hint.
pub fn format_bedrock_error(status: u16, body: &str) -> String {
    let core = format!("{status}: {body}");
    let data_retention_hint = if regex::Regex::new(r"(?i)data retention mode").unwrap().is_match(body) {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    format!("{core}{data_retention_hint}")
}

/// `streamSimple`: composes Bedrock options from simple options (upstream
/// `streamSimple` including max-token/thinking-budget adjustment).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let base = BedrockOptions {
        base: options.base.clone(),
        region: None,
        profile: None,
        tool_choice: options.tool_choice.map(|tc| match tc {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
        }),
        reasoning: None,
        thinking_budgets: options.thinking_budgets.clone(),
        interleaved_thinking: None,
        thinking_display: None,
        request_metadata: None,
        max_tokens: None,
    };

    let Some(reasoning) = options.reasoning else {
        return stream(model, context, client, api_key, &BedrockOptions { reasoning: None, ..base });
    };
    let reasoning_str = reasoning.as_str();

    if is_anthropic_claude_model(model) {
        if supports_adaptive_thinking(model) {
            return stream(model, context, client, api_key, &BedrockOptions {
                reasoning: Some(reasoning_str.to_string()),
                thinking_budgets: options.thinking_budgets.clone(),
                ..base
            });
        }
        let (max_tokens, thinking_budget) = adjust_max_tokens_for_thinking(
            options.base.max_tokens,
            model.max_tokens,
            reasoning_str,
            options.thinking_budgets.as_ref(),
        );
        // Context clamping (upstream clampMaxTokensToContext).
        let clamped = clamp_max_tokens_to_context(model, context, max_tokens);
        let budget_level = if reasoning_str == "xhigh" || reasoning_str == "max" { "high" } else { reasoning_str };
        let mut budgets = options.thinking_budgets.clone().unwrap_or_default();
        match budget_level {
            "minimal" => budgets.minimal = Some(thinking_budget.min(clamped.saturating_sub(1024))),
            "low" => budgets.low = Some(thinking_budget.min(clamped.saturating_sub(1024))),
            "medium" => budgets.medium = Some(thinking_budget.min(clamped.saturating_sub(1024))),
            _ => budgets.high = Some(thinking_budget.min(clamped.saturating_sub(1024))),
        }
        return stream(model, context, client, api_key, &BedrockOptions {
            max_tokens: Some(clamped),
            reasoning: Some(reasoning_str.to_string()),
            thinking_budgets: Some(budgets),
            ..base
        });
    }

    stream(model, context, client, api_key, &BedrockOptions {
        reasoning: Some(reasoning_str.to_string()),
        thinking_budgets: options.thinking_budgets.clone(),
        ..base
    })
}

/// `clampMaxTokensToContext`: keep at least 4096 safety tokens for the
/// answer; estimate context tokens with the shared estimator.
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    const CONTEXT_SAFETY_TOKENS: u64 = 4096;
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let available = model
        .context_window
        .saturating_sub(crate::utils::estimate::estimate_context_tokens(context).tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(available.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Context;
    use serde_json::json;

    fn base_model() -> Model {
        let mut m = Model::new(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "Claude Sonnet 4.5 (US)",
            "bedrock-converse-stream",
            "amazon-bedrock",
        );
        m.base_url = "https://bedrock-runtime.us-east-1.amazonaws.com".to_string();
        m.reasoning = true;
        m.input = vec![crate::model::ModelInput::Text, crate::model::ModelInput::Image];
        m.cost = crate::model::ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75, tiers: None };
        m.context_window = 200_000;
        m.max_tokens = 64_000;
        m.compat = Some(json!({ "supportsStrictMode": true }));
        m
    }

    fn user_ctx(text: &str) -> Context {
        Context {
            system_prompt: None,
            messages: vec![Message::User(UserContent::string(text, 1))],
            tools: vec![],
        }
    }

    #[test]
    fn sigv4_matches_aws_documented_get_example() {
        // AWS SigV4 test suite "GET iam" example:
        // https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
        let now_ms = 1_440_938_160_000; // 2015-08-30T12:36:00Z
        let headers = sign_aws4_request_with_headers(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            "iam.amazonaws.com",
            b"",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            "us-east-1",
            "iam",
            now_ms,
            &[("content-type", "application/x-www-form-urlencoded; charset=utf-8")],
        );
        let auth = headers.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(
            auth.contains("Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"),
            "got auth: {auth}"
        );
        assert!(auth.contains("Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date"));
    }

    #[test]
    fn sigv4_signs_payload_hash() {
        let headers = sign_aws4_request(
            "POST",
            "/model/us.anthropic.claude-sonnet-4-5-20250929-v1:0/converse-stream",
            "",
            "bedrock-runtime.us-east-1.amazonaws.com",
            b"{\"modelId\":\"x\"}",
            "AKID",
            "SECRET",
            None,
            "us-east-1",
            "bedrock",
            1_700_000_000_000,
        );
        assert!(headers.iter().any(|(k, _)| k == "x-amz-date"));
        assert!(headers.iter().any(|(k, v)| k == "authorization" && v.contains("/us-east-1/bedrock/aws4_request")));
    }

    #[test]
    fn sigv4_includes_session_token_when_present() {
        let headers = sign_aws4_request(
            "POST",
            "/",
            "",
            "host.example.com",
            b"",
            "AKID",
            "SECRET",
            Some("TOKEN"),
            "us-east-1",
            "bedrock",
            1_700_000_000_000,
        );
        assert!(headers.iter().any(|(k, v)| k == "x-amz-security-token" && v == "TOKEN"));
        let auth = headers.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(auth.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"), "got {auth}");
    }

    #[test]
    fn convert_messages_blank_user_string_becomes_placeholder() {
        let model = base_model();
        let mut ctx = user_ctx("   ");
        let out = convert_messages(&ctx, &model, "none", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], json!([{ "text": "<empty>" }]));
        let _ = &mut ctx;
    }

    #[test]
    fn convert_messages_skips_unknown_user_blocks() {
        let model = base_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserContent::RoleUser {
                content: UserContentBody::Blocks(vec![
                    ContentBlock::text("hello"),
                    ContentBlock::image("aGVsbG8=", "image/png"),
                ]),
                timestamp: 1,
            })],
            tools: vec![],
        };
        let out = convert_messages(&ctx, &model, "none", None);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["content"],
            json!([
                { "text": "hello" },
                { "image": { "source": { "bytes": "aGVsbG8=" }, "format": "png" } }
            ])
        );
    }

    #[test]
    fn convert_messages_blank_tool_result_becomes_placeholder() {
        let model = base_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::ToolResult(ToolResultMessage::new(
                "tool-1", "tool", vec![ContentBlock::text("")], false,
            ))],
            tools: vec![],
        };
        let out = convert_messages(&ctx, &model, "none", None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"][0]["toolResult"]["content"], json!([{ "text": "<empty>" }]));
        assert_eq!(out[0]["content"][0]["toolResult"]["status"], json!("success"));
    }

    #[test]
    fn convert_messages_combines_consecutive_tool_results() {
        let model = base_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![
                Message::ToolResult(ToolResultMessage::text("t1", "read", "a", false)),
                Message::ToolResult(ToolResultMessage::text("t2", "read", "b", true)),
                Message::User(UserContent::string("continue", 3)),
            ],
            tools: vec![],
        };
        let out = convert_messages(&ctx, &model, "none", None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], json!("user"));
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"][1]["toolResult"]["toolUseId"], json!("t2"));
        assert_eq!(out[0]["content"][1]["toolResult"]["status"], json!("error"));
    }

    #[test]
    fn convert_messages_replays_redacted_reasoning_before_tool_use() {
        let model = base_model();
        let mut assistant = AssistantMessage::new();
        *assistant.content_mut() = vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: Some("cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF".to_string()),
                redacted: Some(true),
            },
            ContentBlock::tool_call("tool-1", "read", json!({ "path": "/tmp/a.txt" })),
            ContentBlock::text("done"),
        ];
        assistant.set_api_provider_model("bedrock-converse-stream", "amazon-bedrock", &model.id);
        let ctx = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserContent::string("read the file", 1)),
                Message::Assistant(assistant),
                Message::User(UserContent::string("continue", 3)),
            ],
            tools: vec![],
        };
        let out = convert_messages(&ctx, &model, "none", None);
        let assistant_msg = out.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(
            assistant_msg["content"][0],
            json!({
                "reasoningContent": {
                    "redactedContent": "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF"
                }
            })
        );
        assert_eq!(assistant_msg["content"][1]["toolUse"]["toolUseId"], json!("tool-1"));
        assert_eq!(assistant_msg["content"][2]["text"], json!("done"));
    }

    #[test]
    fn convert_messages_strips_empty_property_names_from_replayed_input() {
        let model = base_model();
        let mut assistant = AssistantMessage::new();
        *assistant.content_mut() = vec![ContentBlock::tool_call(
            "tool-1",
            "edit",
            json!({
                "path": "/workspace/foobar/file.js",
                "edits": [
                    { "oldText": "first", "newText": "updated first" },
                    { "oldText": "second", "newText": "updated second", "": "" }
                ]
            }),
        )];
        assistant.set_api_provider_model("bedrock-converse-stream", "amazon-bedrock", &model.id);
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(assistant)],
            tools: vec![],
        };
        let out = convert_messages(&ctx, &model, "none", None);
        let input = out[0]["content"][0]["toolUse"]["input"].clone();
        assert_eq!(
            &input,
            &json!({
                "path": "/workspace/foobar/file.js",
                "edits": [
                    { "oldText": "first", "newText": "updated first" },
                    { "oldText": "second", "newText": "updated second" }
                ]
            })
        );
    }

    #[test]
    fn cache_points_added_for_supported_claude_models() {
        let model = base_model();
        let ctx = user_ctx("hello");
        // short: cache point without ttl on the last user message.
        let out = convert_messages(&ctx, &model, "short", None);
        let last = out.last().unwrap();
        assert_eq!(last["content"].as_array().unwrap().last().unwrap()["cachePoint"], json!({ "type": "default" }));
        // long: ttl ONE_HOUR.
        let out = convert_messages(&ctx, &model, "long", None);
        let last = out.last().unwrap();
        assert_eq!(
            last["content"].as_array().unwrap().last().unwrap()["cachePoint"],
            json!({ "type": "default", "ttl": "ONE_HOUR" })
        );
        // none: no cache point.
        let out = convert_messages(&ctx, &model, "none", None);
        let last = out.last().unwrap();
        assert!(last["content"].as_array().unwrap().iter().all(|b| b.get("cachePoint").is_none()));
    }

    #[test]
    fn system_prompt_cache_point() {
        let model = base_model();
        let ctx = Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![],
            tools: vec![],
        };
        let sys = build_system_prompt(ctx.system_prompt.as_deref(), &model, "short", None).unwrap();
        assert_eq!(sys[0], json!({ "text": "You are helpful." }));
        assert_eq!(sys[1]["cachePoint"], json!({ "type": "default" }));
        let sys_none = build_system_prompt(ctx.system_prompt.as_deref(), &model, "none", None).unwrap();
        assert_eq!(sys_none.as_array().unwrap().len(), 1);
    }

    #[test]
    fn thinking_payload_adaptive_for_opus48() {
        let mut model = base_model();
        model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
        model.name = "Claude Opus 4.8 (Global)".to_string();
        let options = BedrockOptions { reasoning: Some("high".to_string()), ..Default::default() };
        let fields = build_additional_model_request_fields(&model, &options).unwrap();
        assert_eq!(fields["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
        assert_eq!(fields["output_config"], json!({ "effort": "high" }));
        assert!(fields.get("anthropic_beta").is_none());
    }

    #[test]
    fn thinking_payload_xhigh_for_opus48() {
        let mut model = base_model();
        model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
        model.name = "Claude Opus 4.8 (Global)".to_string();
        let options = BedrockOptions { reasoning: Some("xhigh".to_string()), ..Default::default() };
        let fields = build_additional_model_request_fields(&model, &options).unwrap();
        assert_eq!(fields["output_config"], json!({ "effort": "xhigh" }));
    }

    #[test]
    fn thinking_payload_fable5_adaptive() {
        let mut model = base_model();
        model.id = "global.anthropic.claude-fable-5".to_string();
        model.name = "Claude Fable 5".to_string();
        model.compat = None;
        let options = BedrockOptions { reasoning: Some("high".to_string()), ..Default::default() };
        let fields = build_additional_model_request_fields(&model, &options).unwrap();
        assert_eq!(fields["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
    }

    #[test]
    fn thinking_payload_budget_for_sonnet45_with_interleaved_beta() {
        let mut model = base_model();
        model.id = "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
        model.name = "Claude Sonnet 4.5 (US)".to_string();
        let options = BedrockOptions { reasoning: Some("high".to_string()), ..Default::default() };
        let fields = build_additional_model_request_fields(&model, &options).unwrap();
        assert_eq!(fields["thinking"], json!({ "type": "enabled", "budget_tokens": 16384, "display": "summarized" }));
        assert_eq!(fields["anthropic_beta"], json!(["interleaved-thinking-2025-05-14"]));
    }

    #[test]
    fn thinking_payload_omits_display_for_govcloud() {
        let mut model = base_model();
        model.id = "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
        model.name = "Claude Sonnet 4.5 (GovCloud)".to_string();
        let options = BedrockOptions { reasoning: Some("high".to_string()), ..Default::default() };
        let fields = build_additional_model_request_fields(&model, &options).unwrap();
        assert_eq!(fields["thinking"], json!({ "type": "enabled", "budget_tokens": 16384 }));
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason(Some("end_turn")), (StopReason::Stop, None));
        assert_eq!(map_stop_reason(Some("stop_sequence")), (StopReason::Stop, None));
        assert_eq!(map_stop_reason(Some("max_tokens")), (StopReason::Length, None));
        assert_eq!(map_stop_reason(Some("model_context_window_exceeded")), (StopReason::Length, None));
        assert_eq!(map_stop_reason(Some("tool_use")), (StopReason::ToolUse, None));
        let (reason, msg) = map_stop_reason(Some("rating_card"));
        assert_eq!(reason, StopReason::Error);
        assert_eq!(msg.unwrap(), "Provider stopped with: rating_card");
    }

    #[test]
    fn endpoint_resolution_prefers_custom_and_region() {
        let _guard = ENV_LOCK.lock().unwrap();
        let model = base_model();
        let options = BedrockOptions::default();
        let config = resolve_config(&model, &options, None).unwrap();
        assert_eq!(config.region, "us-east-1");
        assert_eq!(config.endpoint, "https://bedrock-runtime.us-east-1.amazonaws.com");

        // Custom endpoint always used.
        let mut custom = model.clone();
        custom.base_url = "https://bedrock-vpc.example.com".to_string();
        let config = resolve_config(&custom, &options, None).unwrap();
        assert_eq!(config.endpoint, "https://bedrock-vpc.example.com");
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn endpoint_resolution_uses_configured_region_for_standard_endpoint() {
        let _guard = ENV_LOCK.lock().unwrap();
        let model = base_model();
        unsafe { std::env::set_var("AWS_REGION", "us-east-2"); }
        let config = resolve_config(&model, &BedrockOptions::default(), None).unwrap();
        assert_eq!(config.region, "us-east-2");
        // Standard endpoint template uses the configured region (upstream
        // leaves config.endpoint unset and the SDK resolves the region).
        assert_eq!(config.endpoint, "https://bedrock-runtime.us-east-2.amazonaws.com");
        unsafe { std::env::remove_var("AWS_REGION"); }
    }

    #[test]
    fn arn_region_extracted() {
        assert_eq!(
            arn_region("arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123").unwrap(),
            "us-west-2"
        );
        assert_eq!(
            arn_region("arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123").unwrap(),
            "us-gov-west-1"
        );
    }

    #[test]
    fn bearer_token_used_when_api_key_present() {
        let model = base_model();
        let config = resolve_config(&model, &BedrockOptions::default(), Some("bedrock-api-key")).unwrap();
        assert_eq!(config.bearer_token.as_deref(), Some("bedrock-api-key"));
        assert_eq!(config.access_key, None);
    }

    #[test]
    fn ambient_credentials_loaded() {
        let model = base_model();
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "secretexample");
        }
        let config = resolve_config(&model, &BedrockOptions::default(), None).unwrap();
        assert_eq!(config.access_key.as_deref(), Some("AKIAEXAMPLE"));
        assert_eq!(config.secret_key.as_deref(), Some("secretexample"));
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
    }

    #[test]
    fn adjust_max_tokens_for_thinking_caps_budget() {
        let (max_tokens, budget) = adjust_max_tokens_for_thinking(None, 64000, "high", None);
        assert_eq!(max_tokens, 64000);
        assert_eq!(budget, 16384);

        let (max_tokens, budget) = adjust_max_tokens_for_thinking(Some(1000), 64000, "high", None);
        assert_eq!(max_tokens, 17384);
        assert_eq!(budget, 16384);

        // maxTokens <= thinkingBudget -> clamp at maxTokens-1024.
        let (max_tokens, budget) = adjust_max_tokens_for_thinking(Some(500), 64000, "high", None);
        assert_eq!(max_tokens, 16884);
        assert_eq!(budget, 16384);

        // maxTokens <= thinkingBudget -> clamp at maxTokens-1024.
        let (max_tokens, budget) = adjust_max_tokens_for_thinking(Some(0), 64000, "high", None);
        assert_eq!(max_tokens, 16384);
        assert_eq!(budget, 15360); // clamped to maxTokens - 1024
    }

    // ------------------------------------------------------------------
    // Eventstream decoding
    // ------------------------------------------------------------------

    fn build_frame(event_type: &str, payload_json: &Value) -> Vec<u8> {
        // headers: :message-type=event (string), :event-type=<event_type>
        let mut headers = Vec::new();
        let mut push_header = |name: &str, value: &str| {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(6); // string
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        };
        push_header(":message-type", "event");
        push_header(":event-type", event_type);
        let payload = serde_json::to_vec(payload_json).unwrap();
        let total_length = 16 + headers.len() + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0x00, 0xC0, 0xDE, 0x00]);
        frame.extend_from_slice(&(total_length as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        let message_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        frame
    }

    #[test]
    fn decodes_eventstream_frames() {
        let payload = json!({ "messageStart": { "role": "assistant" } });
        let mut bytes = build_frame("messageStart", &payload);
        let frames = decode_eventstream_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "messageStart");
        assert_eq!(frames[0].payload.as_ref().unwrap()["messageStart"]["role"], json!("assistant"));

        // Two frames concatenated.
        let payload2 = json!({ "messageStop": { "stopReason": "end_turn" } });
        bytes.extend_from_slice(&build_frame("messageStop", &payload2));
        let frames = decode_eventstream_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);

        // Corrupt CRC is rejected.
        let mut bad = build_frame("messageStart", &payload);
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        assert!(decode_eventstream_frames(&bad).is_err());
    }

    // ------------------------------------------------------------------
    // Full-stream E2E with a local server
    // ------------------------------------------------------------------

    async fn start_eventstream_server(frames: &[Vec<u8>], status: u16) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let requests_handle = requests.clone();
        let body: Vec<u8> = frames.iter().flat_map(|f| f.iter().cloned()).collect();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 { break; }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            requests_handle.lock().unwrap().push(text);
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/vnd.amazon.eventstream\r\nx-amzn-requestid: req-123\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            ).await;
            let _ = socket.write_all(&body).await;
        });
        (format!("http://{addr}"), requests)
    }

    #[tokio::test]
    async fn full_stream_text_and_stop() {
        let frames = vec![
            build_frame("messageStart", &json!({ "messageStart": { "role": "assistant" } })),
            build_frame("contentBlockDelta", &json!({
                "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "text": "Hello" } }
            })),
            build_frame("contentBlockStop", &json!({ "contentBlockStop": { "contentBlockIndex": 0 } })),
            build_frame("messageStop", &json!({ "messageStop": { "stopReason": "end_turn" } })),
            build_frame("metadata", &json!({
                "metadata": { "usage": { "inputTokens": 10, "outputTokens": 5, "cacheReadInputTokens": 2, "cacheWriteInputTokens": 0, "totalTokens": 15 } }
            })),
        ];
        let (base_url, requests) = start_eventstream_server(&frames, 200).await;
        let mut model = base_model();
        model.base_url = base_url.clone();
        let ctx = user_ctx("hello");
        let client = reqwest::Client::new();
        let mut options = BedrockOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    env: Some({
                        let mut e = crate::types::ProviderEnv::new();
                        e.insert("AWS_BEDROCK_SKIP_AUTH".to_string(), "1".to_string());
                        e
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: None,
            ..Default::default()
        };
        let s = stream(&model, &ctx, client, None, &options);
        let (events, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert_eq!(message.content().len(), 1);
        let mut text = String::new();
        if let Some(ContentBlock::Text { text: t, .. }) = message.content().first() {
            text = t.clone();
        }
        assert_eq!(text, "Hello");
        assert_eq!(message.usage().map(|u| u.input), Some(10)); // upstream keeps inputTokens verbatim
        assert_eq!(message.usage().map(|u| u.cache_read), Some(2));
        assert_eq!(message.usage().map(|u| u.total_tokens), Some(15));
        assert!(events.iter().any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
        // The request was SigV4-signed even with dummy keys (skipAuth path).
        let req = requests.lock().unwrap()[0].clone();
        assert!(req.to_lowercase().contains("authorization: aws4-hmac-sha256"), "got: {req}");
        let _ = &mut options;
    }

    #[tokio::test]
    async fn full_stream_tool_use_json_arguments() {
        let frames = vec![
            build_frame("messageStart", &json!({ "messageStart": { "role": "assistant" } })),
            build_frame("contentBlockStart", &json!({
                "contentBlockStart": { "contentBlockIndex": 1, "start": { "toolUse": { "toolUseId": "tool-1", "name": "edit" } } }
            })),
            build_frame("contentBlockDelta", &json!({
                "contentBlockDelta": { "contentBlockIndex": 1, "delta": { "toolUse": { "input": "{\"path\":\"/workspace/file.js\",\"edits\":[{\"oldText\":\"first\",\"newText\":\"up\",\"\":\"\"}]}" } } }
            })),
            build_frame("contentBlockStop", &json!({ "contentBlockStop": { "contentBlockIndex": 1 } })),
            build_frame("messageStop", &json!({ "messageStop": { "stopReason": "tool_use" } })),
        ];
        let (base_url, _) = start_eventstream_server(&frames, 200).await;
        let mut model = base_model();
        model.base_url = base_url.clone();
        let ctx = user_ctx("Use the tool");
        let client = reqwest::Client::new();
        let options = BedrockOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    env: Some({
                        let mut e = crate::types::ProviderEnv::new();
                        e.insert("AWS_BEDROCK_SKIP_AUTH".to_string(), "1".to_string());
                        e
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: None,
            ..Default::default()
        };
        let s = stream(&model, &ctx, client, None, &options);
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
        let tool = message.content().iter().find_map(|b| match b {
            ContentBlock::ToolCall { id, name, arguments, .. } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        }).unwrap();
        assert_eq!(tool.0, "tool-1");
        assert_eq!(tool.1, "edit");
        assert_eq!(tool.2["path"], json!("/workspace/file.js"));
        assert_eq!(tool.2["edits"][0]["newText"], json!("up"));
    }

    #[tokio::test]
    async fn full_stream_redacted_reasoning_joins_deltas() {
        let redacted_b64 = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";
        let redacted_bytes = base64::engine::general_purpose::STANDARD.decode(redacted_b64).unwrap();
        let (head, tail) = redacted_bytes.split_at(7);
        let head_b64 = base64::engine::general_purpose::STANDARD.encode(head);
        let tail_b64 = base64::engine::general_purpose::STANDARD.encode(tail);
        let frames = vec![
            build_frame("messageStart", &json!({ "messageStart": { "role": "assistant" } })),
            build_frame("contentBlockDelta", &json!({
                "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "reasoningContent": { "redactedContent": head_b64 } } }
            })),
            build_frame("contentBlockDelta", &json!({
                "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "reasoningContent": { "redactedContent": tail_b64 } } }
            })),
            build_frame("contentBlockStop", &json!({ "contentBlockStop": { "contentBlockIndex": 0 } })),
            build_frame("contentBlockDelta", &json!({
                "contentBlockDelta": { "contentBlockIndex": 1, "delta": { "text": "done" } }
            })),
            build_frame("contentBlockStop", &json!({ "contentBlockStop": { "contentBlockIndex": 1 } })),
            build_frame("messageStop", &json!({ "messageStop": { "stopReason": "end_turn" } })),
        ];
        let (base_url, _) = start_eventstream_server(&frames, 200).await;
        let mut model = base_model();
        model.id = "global.openai.gpt-5.6-terra".to_string();
        model.name = "GPT-5.6 Terra (Global)".to_string();
        model.base_url = base_url.clone();
        let ctx = user_ctx("hello");
        let client = reqwest::Client::new();
        let options = BedrockOptions {
            base: StreamOptions {
                base: crate::types::ProviderRequestOptions {
                    env: Some({
                        let mut e = crate::types::ProviderEnv::new();
                        e.insert("AWS_BEDROCK_SKIP_AUTH".to_string(), "1".to_string());
                        e
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: None,
            ..Default::default()
        };
        let s = stream(&model, &ctx, client, None, &options);
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        let types: Vec<&str> = message.content().iter().map(|b| match b {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Thinking { .. } => "thinking",
            _ => "other",
        }).collect();
        assert_eq!(types, vec!["thinking", "text"]);
        let thinking = message.content().iter().find_map(|b| match b {
            ContentBlock::Thinking { thinking, thinking_signature, redacted, .. } => {
                Some((thinking.clone(), thinking_signature.clone(), *redacted))
            }
            _ => None,
        }).unwrap();
        assert_eq!(thinking.0, "[Reasoning redacted]");
        assert_eq!(thinking.1.as_deref(), Some(redacted_b64));
        assert_eq!(thinking.2, Some(true));
    }
}
