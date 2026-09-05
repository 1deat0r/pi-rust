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
    // An empty settings file is a valid empty scope. Keep the early process
    // bootstrap consistent with SettingsManager instead of reporting a JSON
    // parse warning before the normal settings load runs.
    if contents.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(crate::core::settings::strip_bom(&contents))
        .map_err(|error| format!("invalid settings JSON at {}: {error}", path.display()))?;
    let proxy = value.get("httpProxy").and_then(Value::as_str);
    apply_http_proxy_settings(proxy);
    Ok(())
}
/// Proxy variable names honored by the provider HTTP clients, in the same
/// upper-then-lowercase lookup order the underlying client uses.
const PROXY_ENV_VARS: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"];

fn proxy_value_routable(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }
    // Mirror the underlying client's acceptance: an http/https/socks scheme
    // (or none, defaulting to http) plus a non-empty authority carrying a
    // host. Anything else is dropped by the client, which would bypass a
    // mandated proxy without a sound.
    let (scheme, rest) = match value.split_once("://") {
        Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
        None => {
            // Schemeless values default to http but cannot carry userinfo
            // (the client parses those as scheme-relative paths and drops
            // them) and must not contain whitespace.
            if value.contains('@') || value.chars().any(char::is_whitespace) {
                return false;
            }
            (None, value)
        }
    };
    if let Some(scheme) = &scheme {
        if !matches!(
            scheme.as_str(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) {
            return false;
        }
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    if host_port.is_empty() {
        return false;
    }
    let candidate = match &scheme {
        Some(_) => value.to_owned(),
        None => format!("http://{value}"),
    };
    url::Url::parse(&candidate)
        .ok()
        .is_some_and(|url| url.host_str().is_some_and(|host| !host.is_empty()))
}

/// Fail closed on proxy values the HTTP clients would silently ignore.
/// Upstream throws when a request first consults an unparseable proxy URL;
/// the Rust port validates once at startup instead so no request can ever
/// slip past a misconfigured proxy. An explicitly empty value stays
/// harmless (it only blocks the settings bridge, matching upstream).
pub fn validate_proxy_env() -> Result<(), String> {
    for var in PROXY_ENV_VARS {
        if let Some(value) = std::env::var_os(var) {
            let value = value.to_string_lossy();
            if !value.trim().is_empty() && !proxy_value_routable(&value) {
                return Err(format!(
                    "invalid {var} proxy URL {value:?}: the address must be an http(s) URL with a host (optionally user:pass@) or a bare host:port"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use super::{
        apply_global_http_proxy_settings, apply_http_proxy_settings, proxy_value_routable,
        validate_proxy_env, DEFAULT_AUTO_SELECT_FAMILY_ATTEMPT_TIMEOUT_MS,
        DEFAULT_HTTP_IDLE_TIMEOUT_MS,
    };
    use crate::core::settings::parse_http_idle_timeout_ms;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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
    fn proxy_setting_preserves_explicitly_empty_environment() {
        let _guard = env_lock();
        let old_http = std::env::var_os("HTTP_PROXY");
        let old_https = std::env::var_os("HTTPS_PROXY");
        std::env::set_var("HTTP_PROXY", "");
        std::env::set_var("HTTPS_PROXY", "");

        apply_http_proxy_settings(Some("http://settings:7890"));

        assert_eq!(std::env::var("HTTP_PROXY").unwrap(), "");
        assert_eq!(std::env::var("HTTPS_PROXY").unwrap(), "");
        restore("HTTP_PROXY", old_http);
        restore("HTTPS_PROXY", old_https);
    }

    #[test]
    fn global_bootstrap_populates_environment_from_settings() {
        let _guard = env_lock();
        let old_http = std::env::var_os("HTTP_PROXY");
        let old_https = std::env::var_os("HTTPS_PROXY");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        let root = std::env::temp_dir().join(format!(
            "pi-http-dispatcher-proxy-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("settings.json"),
            r#"{"httpProxy":"http://127.0.0.1:7890"}"#,
        )
        .unwrap();

        assert!(apply_global_http_proxy_settings(&root).is_ok());
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
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_validation_accepts_routable_shapes() {
        for value in [
            "http://127.0.0.1:7890",
            "https://proxy.example:8443",
            "http://user:pass@127.0.0.1:7890",
            "  http://127.0.0.1:7890  ",
            "127.0.0.1:7890",
            "proxy.example",
            "socks5://127.0.0.1:1080",
        ] {
            assert!(proxy_value_routable(value), "routable: {value:?}");
        }
    }

    #[test]
    fn proxy_validation_rejects_silently_ignored_shapes() {
        for value in [
            "://bad-proxy-value",
            "http://[::1",
            "not a url at all !!!",
            "http://",
            "ftp://127.0.0.1:21",
            "http:///no-host",
        ] {
            assert!(!proxy_value_routable(value), "unroutable: {value:?}");
        }
        assert!(proxy_value_routable(""));
    }

    #[test]
    fn proxy_validation_names_the_offending_variable() {
        let _guard = env_lock();
        let old = std::env::var_os("HTTP_PROXY");
        std::env::set_var("HTTP_PROXY", "://bad-proxy-value");

        let error = validate_proxy_env().unwrap_err();
        assert!(
            error.contains("HTTP_PROXY") && error.contains("://bad-proxy-value"),
            "diagnostic must name the variable and value: {error}"
        );
        restore("HTTP_PROXY", old);
    }

    #[test]
    fn malformed_settings_reports_the_offending_path() {
        let root = std::env::temp_dir().join(format!(
            "pi-http-dispatcher-malformed-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("settings.json"), "{bad").unwrap();

        let error = apply_global_http_proxy_settings(&root).unwrap_err();
        assert!(
            error.contains("settings.json"),
            "diagnostic must name the file: {error}"
        );
        let _ = fs::remove_dir_all(root);
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

    #[test]
    fn empty_settings_file_is_accepted_by_process_bootstrap() {
        let root = std::env::temp_dir().join(format!(
            "pi-http-dispatcher-empty-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("settings.json"), "").unwrap();

        assert!(apply_global_http_proxy_settings(&root).is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
