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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LatestPiRelease {
    pub version: String,
    #[serde(default, rename = "packageName")]
    pub package_name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: bool,
}

fn parse_version(value: &str) -> Option<ParsedVersion> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let (core, prerelease) = match value.split_once('-') {
        Some((core, suffix)) if !suffix.is_empty() => (core, true),
        Some(_) => return None,
        None => (value, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
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

/// Compare two package versions. Returns `None` for versions outside the
/// semver shape used by the upstream check.
pub fn compare_package_versions(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Some(
        (left.major, left.minor, left.patch)
            .cmp(&(right.major, right.minor, right.patch))
            .then_with(|| right.prerelease.cmp(&left.prerelease)),
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
                return Ok(Some(LatestPiRelease {
                    version: release.version.trim().to_string(),
                    package_name: release.package_name.filter(|v| !v.trim().is_empty()),
                    note: release.note.filter(|v| !v.trim().is_empty()),
                }));
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
}
