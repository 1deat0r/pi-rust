//! HTTP bootstrap and timeout helpers from
//! `packages/coding-agent/src/core/http-dispatcher.ts`.
//!
//! Reqwest owns connection pooling and proxy dispatch in the Rust port, so
//! there is no undici dispatcher object to install. The observable upstream
//! contract that applies to this distribution is preserved: a configured
//! `httpProxy` supplies `HTTP_PROXY` and `HTTPS_PROXY` only when the caller has
//! not already set them, and idle-timeout values use the shared settings
//! parser. The provider facade consumes those environment variables when it
//! builds its clients.

use std::path::Path;

use serde_json::Value;

pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_AUTO_SELECT_FAMILY_ATTEMPT_TIMEOUT_MS: u64 = 2_000;

/// Apply a configured proxy without overriding explicit process environment.
/// This mirrors upstream's nullish assignment semantics, including treating an
/// explicitly empty environment value as present.
pub fn apply_http_proxy_settings(http_proxy: Option<&str>) {
    let Some(proxy) = http_proxy.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };

    if std::env::var_os("HTTP_PROXY").is_none() {
        std::env::set_var("HTTP_PROXY", proxy);
    }
    if std::env::var_os("HTTPS_PROXY").is_none() {
        std::env::set_var("HTTPS_PROXY", proxy);
    }
}

/// Apply the global `httpProxy` setting early enough for auth, package, and
/// mode dispatch. A missing file is a normal first-run state; malformed JSON
/// is returned so the caller can surface a diagnostic without panicking.
pub fn apply_global_http_proxy_settings(agent_dir: impl AsRef<Path>) -> Result<(), String> {
    let path = agent_dir.as_ref().join("settings.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(crate::core::settings::strip_bom(&contents))
        .map_err(|error| format!("invalid settings JSON at {}: {error}", path.display()))?;
    let proxy = value.get("httpProxy").and_then(Value::as_str);
    apply_http_proxy_settings(proxy);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use super::{
        apply_http_proxy_settings, DEFAULT_AUTO_SELECT_FAMILY_ATTEMPT_TIMEOUT_MS,
        DEFAULT_HTTP_IDLE_TIMEOUT_MS,
    };
    use crate::core::settings::parse_http_idle_timeout_ms;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn restore(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn proxy_setting_populates_both_http_variables() {
        let _guard = env_lock();
        let old_http = std::env::var_os("HTTP_PROXY");
        let old_https = std::env::var_os("HTTPS_PROXY");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");

        apply_http_proxy_settings(Some("  http://127.0.0.1:7890  "));

        assert_eq!(
            std::env::var("HTTP_PROXY").unwrap(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            std::env::var("HTTPS_PROXY").unwrap(),
            "http://127.0.0.1:7890"
        );
        restore("HTTP_PROXY", old_http);
        restore("HTTPS_PROXY", old_https);
    }

    #[test]
    fn proxy_setting_preserves_existing_environment_and_ignores_empty_values() {
        let _guard = env_lock();
        let old_http = std::env::var_os("HTTP_PROXY");
        let old_https = std::env::var_os("HTTPS_PROXY");
        std::env::set_var("HTTP_PROXY", "http://env-http:8080");
        std::env::set_var("HTTPS_PROXY", "http://env-https:8080");

        apply_http_proxy_settings(Some("http://settings:7890"));
        apply_http_proxy_settings(Some("   "));

        assert_eq!(std::env::var("HTTP_PROXY").unwrap(), "http://env-http:8080");
        assert_eq!(
            std::env::var("HTTPS_PROXY").unwrap(),
            "http://env-https:8080"
        );
        restore("HTTP_PROXY", old_http);
        restore("HTTPS_PROXY", old_https);
    }

    #[test]
    fn timeout_defaults_and_parser_match_upstream_choices() {
        assert_eq!(DEFAULT_HTTP_IDLE_TIMEOUT_MS, 300_000);
        assert_eq!(DEFAULT_AUTO_SELECT_FAMILY_ATTEMPT_TIMEOUT_MS, 2_000);
        assert_eq!(parse_http_idle_timeout_ms(&json!("disabled")), Some(0));
        assert_eq!(parse_http_idle_timeout_ms(&json!(1.99)), Some(1));
        assert_eq!(parse_http_idle_timeout_ms(&json!("")), None);
        assert_eq!(parse_http_idle_timeout_ms(&json!(-1)), None);
    }
}
