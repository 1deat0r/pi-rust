//! Version-check helpers — port of `packages/coding-agent/src/utils/version-check.ts`.
//!
//! The check is deliberately best-effort for normal startup: offline mode,
//! an explicit skip flag, malformed responses, and network failures all
//! resolve to no notice. The update command uses the same parser with retries
//! and surfaces failures to its caller.

use std::cmp::Ordering;
use std::time::Duration;

use serde::Deserialize;

pub const LATEST_VERSION_URL: &str = "https://pi.dev/api/latest-version";
pub const DEFAULT_VERSION_CHECK_TIMEOUT_MS: u64 = 10_000;
const RETRYABLE_STATUS_CODES: [reqwest::StatusCode; 7] = [
    reqwest::StatusCode::REQUEST_TIMEOUT,
    reqwest::StatusCode::TOO_EARLY,
    reqwest::StatusCode::TOO_MANY_REQUESTS,
    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
    reqwest::StatusCode::BAD_GATEWAY,
    reqwest::StatusCode::SERVICE_UNAVAILABLE,
    reqwest::StatusCode::GATEWAY_TIMEOUT,
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LatestPiRelease {
    pub version: String,
    #[serde(default, rename = "packageName")]
    pub package_name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<String>,
}

fn parse_version(value: &str) -> Option<ParsedVersion> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let value = value
        .split_once('+')
        .map(|(value, _)| value)
        .unwrap_or(value);
    let (core, prerelease) = match value.split_once('-') {
        Some((core, suffix)) if !suffix.is_empty() => {
            let identifiers = suffix.split('.').map(str::to_string).collect::<Vec<_>>();
            if identifiers.iter().any(|identifier| {
                identifier.is_empty()
                    || !identifier
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
                    || (identifier.len() > 1
                        && identifier.starts_with('0')
                        && identifier
                            .chars()
                            .all(|character| character.is_ascii_digit()))
            }) {
                return None;
            }
            (core, identifiers)
        }
        Some(_) => return None,
        None => (value, Vec::new()),
    };
    let mut parts = core.split('.');
    let parse_component = |component: &str| {
        if component.is_empty()
            || (component.len() > 1
                && component.starts_with('0')
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit()))
            || !component
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            return None;
        }
        component.parse().ok()
    };
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = parse_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(ParsedVersion {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn compare_prerelease(left: &[String], right: &[String]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (left_identifier, right_identifier) in left.iter().zip(right) {
                let left_numeric = left_identifier.chars().all(|c| c.is_ascii_digit());
                let right_numeric = right_identifier.chars().all(|c| c.is_ascii_digit());
                let ordering = match (left_numeric, right_numeric) {
                    (true, true) => left_identifier
                        .parse::<u64>()
                        .ok()
                        .cmp(&right_identifier.parse::<u64>().ok()),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left_identifier.cmp(right_identifier),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            left.len().cmp(&right.len())
        }
    }
}

/// Compare two package versions. Returns `None` for versions outside the
/// semver shape used by the upstream check.
pub fn compare_package_versions(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Some(
        (left.major, left.minor, left.patch)
            .cmp(&(right.major, right.minor, right.patch))
            .then_with(|| compare_prerelease(&left.prerelease, &right.prerelease)),
    )
}

pub fn is_newer_package_version(candidate: &str, current: &str) -> bool {
    compare_package_versions(candidate, current)
        .map(|ordering| ordering == Ordering::Greater)
        .unwrap_or_else(|| candidate.trim() != current.trim())
}

fn endpoint() -> String {
    std::env::var("PI_VERSION_CHECK_URL").unwrap_or_else(|_| LATEST_VERSION_URL.to_string())
}

/// Fetch the latest release. `PI_VERSION_CHECK_URL` is a test seam and does
/// not change the production endpoint.
pub async fn get_latest_pi_release(
    current_version: &str,
    retry: bool,
) -> Result<Option<LatestPiRelease>, String> {
    if std::env::var_os("PI_OFFLINE").is_some() {
        return Ok(None);
    }
    match tokio::time::timeout(
        Duration::from_millis(DEFAULT_VERSION_CHECK_TIMEOUT_MS),
        fetch_latest_pi_release(current_version, retry),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err("version check timed out".to_string()),
    }
}

async fn fetch_latest_pi_release(
    current_version: &str,
    retry: bool,
) -> Result<Option<LatestPiRelease>, String> {
    let attempts = if retry { 3 } else { 1 };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(DEFAULT_VERSION_CHECK_TIMEOUT_MS))
        .build()
        .map_err(|e| format_version_check_error(&e))?;
    let mut last_error = None;
    for attempt in 0..attempts {
        let result = client
            .get(endpoint())
            .header("accept", "application/json")
            .header("user-agent", format!("pi/{current_version}"))
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                let release = response
                    .json::<LatestPiRelease>()
                    .await
                    .map_err(|e| format_version_check_error(&e))?;
                if release.version.trim().is_empty() {
                    return Ok(None);
                }
                let package_name = release.package_name.and_then(|value| {
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                });
                let note = release.note.and_then(|value| {
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                });
                return Ok(Some(LatestPiRelease {
                    version: release.version.trim().to_string(),
                    package_name,
                    note,
                }));
            }
            Ok(response)
                if retry
                    && RETRYABLE_STATUS_CODES.contains(&response.status())
                    && attempt + 1 < attempts =>
            {
                drop(response);
            }
            Ok(_) => return Ok(None),
            Err(error) => {
                last_error = Some(format_version_check_error(&error));
                if attempt + 1 < attempts {
                    tokio::time::sleep(Duration::from_millis(100 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "version check failed".to_string()))
}

/// Convenience form matching upstream `getLatestPiVersion`.
pub async fn get_latest_pi_version(
    current_version: &str,
    retry: bool,
) -> Result<Option<String>, String> {
    Ok(get_latest_pi_release(current_version, retry)
        .await?
        .map(|release| release.version))
}

pub async fn check_for_new_pi_version(current_version: &str) -> Option<LatestPiRelease> {
    if std::env::var_os("PI_SKIP_VERSION_CHECK").is_some() {
        return None;
    }
    match get_latest_pi_release(current_version, false).await {
        Ok(Some(release)) if is_newer_package_version(&release.version, current_version) => {
            Some(release)
        }
        _ => None,
    }
}

pub fn format_version_check_error(error: &dyn std::error::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        crate::core::environment_test_lock().await
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.as_bytes().to_vec();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        (format!("http://{address}/latest-version"), handle)
    }

    #[test]
    fn compares_release_and_prerelease_versions() {
        assert_eq!(
            compare_package_versions("0.85.0", "0.84.2"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_package_versions("0.84.2-beta.1", "0.84.2"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_package_versions("0.84.2-beta.2", "0.84.2-beta.10"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_package_versions("0.84.2+build.4", "0.84.2+build.5"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_package_versions("v0.84.2", "0.84.2"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_package_versions("latest", "0.84.2"), None);
    }

    #[test]
    fn invalid_versions_fall_back_to_trimmed_string_comparison() {
        assert!(is_newer_package_version("next", "current"));
        assert!(!is_newer_package_version("same", "same"));
    }

    #[test]
    fn release_deserializes_optional_fields() {
        let release: LatestPiRelease =
            serde_json::from_value(serde_json::json!({"version":"0.85.0"})).unwrap();
        assert_eq!(release.version, "0.85.0");
        assert!(release.package_name.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn latest_release_fetches_and_normalizes_optional_fields() {
        let _lock = env_lock().await;
        let _offline = EnvGuard::remove("PI_OFFLINE");
        let (url, server) = serve_once(
            "200 OK",
            r#"{"version":" v0.85.0 ","packageName":" @demo/pi ","note":"  update soon  "}"#,
        );
        let _endpoint = EnvGuard::set("PI_VERSION_CHECK_URL", &url);
        let release = get_latest_pi_release("0.84.2", false)
            .await
            .unwrap()
            .unwrap();
        server.join().unwrap();
        assert_eq!(release.version, "v0.85.0");
        assert_eq!(release.package_name.as_deref(), Some("@demo/pi"));
        assert_eq!(release.note.as_deref(), Some("update soon"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn offline_and_skip_flags_short_circuit_network_checks() {
        let _lock = env_lock().await;
        let _endpoint = EnvGuard::set("PI_VERSION_CHECK_URL", "http://[::1");
        let _offline = EnvGuard::set("PI_OFFLINE", "1");
        assert_eq!(get_latest_pi_release("0.84.2", false).await.unwrap(), None);
        drop(_offline);
        let _skip = EnvGuard::set("PI_SKIP_VERSION_CHECK", "1");
        assert!(check_for_new_pi_version("0.84.2").await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_endpoint_surfaces_update_fetch_failure() {
        let _lock = env_lock().await;
        let _offline = EnvGuard::remove("PI_OFFLINE");
        let _endpoint = EnvGuard::set("PI_VERSION_CHECK_URL", "http://[::1");
        let error = get_latest_pi_release("0.84.2", false).await.unwrap_err();
        assert!(!error.is_empty());
    }
}
