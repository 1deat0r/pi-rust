//! Install/update telemetry gate — port of
//! `packages/coding-agent/src/core/telemetry.ts`.
//!
//! `PI_TELEMETRY` overrides the `enableInstallTelemetry` setting: set to a
//! truthy value (`1`/`true`/`yes`, case-insensitive) it enables; set to
//! anything else (`0`/`false`/`no`) it disables; unset it defers to the
//! setting. The gate controls anonymous install/update reporting to
//! `pi.dev` and the optional provider attribution headers.

use crate::core::settings::SettingsManager;

/// Upstream install-report endpoint. Tests may override this with
/// `PI_INSTALL_TELEMETRY_URL` without changing the production contract.
pub const INSTALL_TELEMETRY_URL: &str = "https://pi.dev/api/report-install";
pub const INSTALL_TELEMETRY_TIMEOUT_MS: u64 = 5_000;
const INSTALL_TELEMETRY_ATTEMPTS: usize = 3;

/// Upstream `isTruthyEnvFlag`: only `1`, `true`, or `yes` (case-insensitive)
/// count as enabled.
fn is_truthy_env_flag(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

/// Upstream `isInstallTelemetryEnabled`. `telemetry_env` defaults to the
/// `PI_TELEMETRY` process env.
pub fn is_install_telemetry_enabled(
    settings: &SettingsManager,
    telemetry_env: Option<&str>,
) -> bool {
    match telemetry_env {
        Some(value) => is_truthy_env_flag(value),
        None => settings.get_enable_install_telemetry(),
    }
}

/// Convenience: resolve telemetry gating from the process environment.
pub fn is_install_telemetry_enabled_from_env(settings: &SettingsManager) -> bool {
    is_install_telemetry_enabled(settings, std::env::var("PI_TELEMETRY").ok().as_deref())
}

fn install_telemetry_endpoint() -> String {
    std::env::var("PI_INSTALL_TELEMETRY_URL").unwrap_or_else(|_| INSTALL_TELEMETRY_URL.to_string())
}

/// Match the upstream `getPiUserAgent` shape while identifying the Rust
/// runtime rather than Bun/Node.
pub fn pi_user_agent(version: &str) -> String {
    format!(
        "pi/{version} ({}; rust/{}; {})",
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    )
}

/// Best-effort install/update report. The interactive caller launches this
/// in the background, so network failure is never surfaced to the user or
/// allowed to delay TUI startup. A short bounded retry loop covers transient
/// transport/5xx failures while retaining the upstream five-second budget.
pub async fn report_install_telemetry(version: &str, enabled: bool) -> Result<(), String> {
    if std::env::var_os("PI_OFFLINE").is_some() || !enabled {
        return Ok(());
    }

    let request = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(
                INSTALL_TELEMETRY_TIMEOUT_MS,
            ))
            .build()
            .map_err(|e| format!("create install telemetry client: {e}"))?;
        let endpoint = install_telemetry_endpoint();
        let mut last_error = None;
        for attempt in 0..INSTALL_TELEMETRY_ATTEMPTS {
            let result = client
                .get(&endpoint)
                .query(&[("version", version)])
                .header("User-Agent", pi_user_agent(version))
                .send()
                .await;
            match result {
                Ok(response)
                    if !response.status().is_server_error()
                        && response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS =>
                {
                    // The upstream fetch intentionally does not inspect the
                    // response body or status; reaching the endpoint is the
                    // observable contract.
                    return Ok(());
                }
                Ok(response) => {
                    last_error = Some(format!("install telemetry HTTP {}", response.status()));
                }
                Err(error) => {
                    last_error = Some(format!("install telemetry request: {error}"));
                }
            }
            if attempt + 1 < INSTALL_TELEMETRY_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt as u64 + 1)))
                    .await;
            }
        }
        Err(last_error.unwrap_or_else(|| "install telemetry request failed".to_string()))
    };

    match tokio::time::timeout(
        std::time::Duration::from_millis(INSTALL_TELEMETRY_TIMEOUT_MS),
        request,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err("install telemetry timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(telemetry: bool) -> SettingsManager {
        SettingsManager::in_memory(
            serde_json::from_value(json!({
                "enableInstallTelemetry": telemetry
            }))
            .unwrap(),
        )
    }

    #[test]
    fn unset_env_defers_to_settings() {
        assert!(is_install_telemetry_enabled(&settings(true), None));
        assert!(!is_install_telemetry_enabled(&settings(false), None));
    }

    #[test]
    fn truthy_env_enables_despite_disabled_setting() {
        for v in ["1", "true", "TRUE", "Yes", "YES", "True"] {
            assert!(
                is_install_telemetry_enabled(&settings(false), Some(v)),
                "expected env {v:?} to force-enable"
            );
        }
    }

    #[test]
    fn falsy_env_disables_despite_enabled_setting() {
        for v in ["0", "false", "FALSE", "no", "off", "2", "nonsense"] {
            assert!(
                !is_install_telemetry_enabled(&settings(true), Some(v)),
                "expected env {v:?} to disable"
            );
        }
    }

    #[test]
    fn empty_env_is_treated_as_disabled_only_when_set() {
        // Empty is "set", so it disables (isTruthyEnvFlag("") == false).
        assert!(!is_install_telemetry_enabled(&settings(true), Some("")));
    }

    #[test]
    fn user_agent_matches_upstream_shape() {
        let agent = pi_user_agent("1.2.3");
        assert!(agent.starts_with("pi/1.2.3 ("));
        assert!(agent.contains("rust/"));
        assert!(agent.ends_with(&format!("; {})", std::env::consts::ARCH)));
    }

    struct EnvRestore {
        offline: Option<std::ffi::OsString>,
        endpoint: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture() -> Self {
            Self {
                offline: std::env::var_os("PI_OFFLINE"),
                endpoint: std::env::var_os("PI_INSTALL_TELEMETRY_URL"),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.offline {
                Some(value) => std::env::set_var("PI_OFFLINE", value),
                None => std::env::remove_var("PI_OFFLINE"),
            }
            match &self.endpoint {
                Some(value) => std::env::set_var("PI_INSTALL_TELEMETRY_URL", value),
                None => std::env::remove_var("PI_INSTALL_TELEMETRY_URL"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn report_retries_transient_failures_and_sends_contract_headers() {
        let _lock = crate::core::environment_test_lock().await;
        let _restore = EnvRestore::capture();
        std::env::remove_var("PI_OFFLINE");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        std::env::set_var(
            "PI_INSTALL_TELEMETRY_URL",
            format!("http://{address}/api/report-install"),
        );
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for attempt in 0..INSTALL_TELEMETRY_ATTEMPTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0_u8; 4096];
                let size = stream.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..size]).to_string());
                let status = if attempt + 1 == INSTALL_TELEMETRY_ATTEMPTS {
                    "204 No Content"
                } else {
                    "503 Service Unavailable"
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        report_install_telemetry("1.2.3", true)
            .await
            .expect("report should recover from transient failures");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), INSTALL_TELEMETRY_ATTEMPTS);
        assert!(requests[0].contains("GET /api/report-install?version=1.2.3"));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("user-agent: pi/1.2.3"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn report_is_noop_when_offline_or_disabled() {
        let _lock = crate::core::environment_test_lock().await;
        let _restore = EnvRestore::capture();
        std::env::set_var("PI_OFFLINE", "1");
        std::env::set_var("PI_INSTALL_TELEMETRY_URL", "http://[::1");
        assert!(report_install_telemetry("1.2.3", true).await.is_ok());
        std::env::remove_var("PI_OFFLINE");
        assert!(report_install_telemetry("1.2.3", false).await.is_ok());
    }
}
