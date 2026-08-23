//! Provider attribution headers — port of
//! `packages/coding-agent/src/core/provider-attribution.ts`.
//!
//! Adds attribution headers to provider requests based on the model's
//! provider/host, honoring the `enableInstallTelemetry` setting, and merges
//! session headers for opencode-backed models.

use std::collections::BTreeMap;

use pi_ai::model::Model;
use pi_ai::types::ProviderHeaders;

use crate::core::settings::SettingsManager;
use crate::core::telemetry::is_install_telemetry_enabled;

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// Hostname of a base URL (mirrors upstream `new URL(baseUrl).hostname`).
/// Returns false for malformed URLs.
fn matches_host(base_url: &str, expected_host: &str) -> bool {
    let Some(after_scheme) = base_url.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let hostport = after_scheme.split(['/', '?', '#']).next().unwrap_or_default();
    // Strip userinfo.
    let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
    // Strip port.
    let host = hostport.split(':').next().unwrap_or(hostport);
    host.eq_ignore_ascii_case(expected_host)
}

fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_nvidia_nim_model(model: &Model) -> bool {
    model.provider == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST)
}

fn is_cloudflare_model(model: &Model) -> bool {
    model.provider == "cloudflare-workers-ai"
        || model.provider == "cloudflare-ai-gateway"
        || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST)
}

fn get_default_attribution_headers(
    model: &Model,
    settings: &SettingsManager,
    telemetry_env: Option<&str>,
) -> Option<BTreeMap<String, String>> {
    if !is_install_telemetry_enabled(settings, telemetry_env) {
        return None;
    }
    if is_openrouter_model(model) {
        let mut headers = BTreeMap::new();
        headers.insert("HTTP-Referer".to_string(), "https://pi.dev".to_string());
        headers.insert("X-OpenRouter-Title".to_string(), "pi".to_string());
        headers.insert("X-OpenRouter-Categories".to_string(), "cli-agent".to_string());
        return Some(headers);
    }
    if is_nvidia_nim_model(model) {
        let mut headers = BTreeMap::new();
        headers.insert("X-BILLING-INVOKE-ORIGIN".to_string(), "Pi".to_string());
        return Some(headers);
    }
    if is_cloudflare_model(model) {
        let mut headers = BTreeMap::new();
        headers.insert("User-Agent".to_string(), "pi-coding-agent".to_string());
        return Some(headers);
    }
    None
}

fn get_session_headers(model: &Model, session_id: Option<&str>) -> Option<BTreeMap<String, String>> {
    let session_id = session_id?;
    if model.provider != "opencode"
        && model.provider != "opencode-go"
        && !matches_host(&model.base_url, OPENCODE_HOST)
    {
        return None;
    }
    let mut headers = BTreeMap::new();
    headers.insert("x-opencode-session".to_string(), session_id.to_string());
    headers.insert("x-opencode-client".to_string(), "pi".to_string());
    Some(headers)
}

/// Merge provider attribution headers and additional header sources.
/// Returns `None` when the merged set is empty (upstream behavior).
/// `telemetry_env` is the `PI_TELEMETRY` override (None = defer to setting),
/// mirroring upstream `isInstallTelemetryEnabled(settings, env)`.
pub fn merge_provider_attribution_headers(
    model: &Model,
    settings: &SettingsManager,
    session_id: Option<&str>,
    header_sources: &[Option<&ProviderHeaders>],
    telemetry_env: Option<&str>,
) -> Option<ProviderHeaders> {
    let mut merged: ProviderHeaders = BTreeMap::new();
    if let Some(headers) = get_session_headers(model, session_id) {
        for (name, value) in headers {
            merged.insert(name, Some(value));
        }
    }
    if let Some(headers) = get_default_attribution_headers(model, settings, telemetry_env) {
        for (name, value) in headers {
            merged.insert(name, Some(value));
        }
    }
    for headers in header_sources.iter().flatten() {
        for (name, value) in headers.iter() {
            merged.insert(name.clone(), value.clone());
        }
    }
    if merged.is_empty() { None } else { Some(merged) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(telemetry: bool) -> SettingsManager {
        SettingsManager::in_memory(serde_json::from_value(json!({
            "enableInstallTelemetry": telemetry
        })).unwrap())
    }

    fn model(provider: &str, base_url: &str) -> Model {
        let mut m = Model::new("m", "M", "openai-chat", provider);
        m.base_url = base_url.to_string();
        m
    }

    #[test]
    fn telemetry_disabled_yields_no_default_headers() {
        let settings = settings(false);
        let m = model("openrouter", "https://openrouter.ai/api/v1");
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[], None);
        assert!(merged.is_none());
    }

    #[test]
    fn openrouter_headers_when_enabled() {
        let settings = settings(true);
        let m = model("openrouter", "https://openrouter.ai/api/v1");
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[], None).unwrap();
        assert_eq!(merged.get("HTTP-Referer").unwrap().as_deref(), Some("https://pi.dev"));
        assert_eq!(merged.get("X-OpenRouter-Title").unwrap().as_deref(), Some("pi"));
        assert_eq!(merged.get("X-OpenRouter-Categories").unwrap().as_deref(), Some("cli-agent"));
    }

    #[test]
    fn openrouter_detected_by_base_url() {
        let settings = settings(true);
        let m = model("custom", "https://openrouter.ai/api");
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[], None);
        assert!(merged.is_some(), "provider not openrouter but host is");
    }

    #[test]
    fn nvidia_nim_headers() {
        let settings = settings(true);
        let m = model("nvidia", "https://integrate.api.nvidia.com/v1");
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[], None).unwrap();
        assert_eq!(merged.get("X-BILLING-INVOKE-ORIGIN").unwrap().as_deref(), Some("Pi"));
    }

    #[test]
    fn cloudflare_headers() {
        let settings = settings(true);
        let m = model("cloudflare-workers-ai", "https://api.cloudflare.com");
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[], None).unwrap();
        assert_eq!(merged.get("User-Agent").unwrap().as_deref(), Some("pi-coding-agent"));
    }

    #[test]
    fn session_headers_only_for_opencode() {
        let settings = settings(false);
        let opencode = model("opencode", "https://opencode.ai");
        let merged = merge_provider_attribution_headers(&opencode, &settings, Some("sess-1"), &[], None).unwrap();
        assert_eq!(merged.get("x-opencode-session").unwrap().as_deref(), Some("sess-1"));
        assert_eq!(merged.get("x-opencode-client").unwrap().as_deref(), Some("pi"));

        let generic = model("google", "https://generativelanguage.googleapis.com");
        assert!(merge_provider_attribution_headers(&generic, &settings, Some("sess-1"), &[], None).is_none());

        // No session id → no session headers.
        assert!(merge_provider_attribution_headers(&opencode, &settings, None, &[], None).is_none());
    }

    #[test]
    fn later_source_headers_win() {
        let settings = settings(true);
        let m = model("openrouter", "https://openrouter.ai");
        let mut override_headers = ProviderHeaders::new();
        override_headers.insert("X-OpenRouter-Title".to_string(), Some("custom".to_string()));
        let merged = merge_provider_attribution_headers(&m, &settings, None, &[Some(&override_headers)], None).unwrap();
        assert_eq!(merged.get("X-OpenRouter-Title").unwrap().as_deref(), Some("custom"));
        assert_eq!(merged.get("HTTP-Referer").unwrap().as_deref(), Some("https://pi.dev"));
    }

    #[test]
    fn telemetry_env_override_wins_over_setting() {
        // Setting disabled but PI_TELEMETRY=1 (truthy) → headers present.
        let settings_disabled = settings(false);
        let m = model("openrouter", "https://openrouter.ai/api/v1");
        let merged = merge_provider_attribution_headers(&m, &settings_disabled, None, &[], Some("true"));
        assert!(merged.is_some(), "truthy env should force attribution despite disabled setting");

        // Setting enabled but PI_TELEMETRY=0 (falsy) → headers absent.
        let settings_enabled = settings(true);
        assert!(merge_provider_attribution_headers(&m, &settings_enabled, None, &[], Some("0")).is_none());

        // Env unset → defers to the setting (no override).
        assert!(merge_provider_attribution_headers(&m, &settings_disabled, None, &[], None).is_none());
        assert!(merge_provider_attribution_headers(&m, &settings_enabled, None, &[], None).is_some());
    }
}
