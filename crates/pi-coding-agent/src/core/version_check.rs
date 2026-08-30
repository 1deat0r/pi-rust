//! Version comparison helpers for the separately maintained pi-rust build.
//!
//! pi-rust deliberately does not query the official Pi release service. The
//! compiled Rust binary is updated from this repository, while the package
//! and model catalog commands own their own refresh operations. The pure
//! comparison helpers remain available to package-management code and tests.

use std::cmp::Ordering;
pub const DEFAULT_VERSION_CHECK_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestPiRelease {
    pub version: String,
    pub package_name: Option<String>,
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

/// Compare two package versions. Returns `None` for values outside the
/// semver shape used by the upstream helper.
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

/// The Rust distribution has no upstream release endpoint. The async shape is
/// retained so callers compiled against the old helper fail closed without a
/// network request or a user-visible update notice.
pub async fn get_latest_pi_release(
    _current_version: &str,
    _retry: bool,
) -> Result<Option<LatestPiRelease>, String> {
    Ok(None)
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

pub async fn check_for_new_pi_version(_current_version: &str) -> Option<LatestPiRelease> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
}
