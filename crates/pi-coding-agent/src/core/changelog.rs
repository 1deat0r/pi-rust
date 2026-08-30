//! Changelog parsing and link normalization.
//!
//! This is the Rust port of `packages/coding-agent/src/utils/changelog.ts`.
//! The release notes are embedded in the binary so an installed `pi` behaves
//! the same as a source checkout, while `PI_CHANGELOG_PATH` remains available
//! for development and packaging tests.

use std::path::Path;

use regex::{Captures, Regex};

const GITHUB_REPO: &str = "earendil-works/pi";
const CHANGELOG_LINK_BASE_PATH: &str = "packages/coding-agent";
const INLINE_MARKDOWN_LINK_PATTERN: &str = r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub content: String,
}

impl ChangelogEntry {
    pub fn version(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn parse_version_header(line: &str) -> Option<(u64, u64, u64)> {
    let rest = line.strip_prefix("## ")?.trim_start();
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    let mut digits = rest.split(|character: char| !character.is_ascii_digit() && character != '.');
    let token = digits.next()?.trim_end_matches('.');
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Parse `## [x.y.z]` entries, matching the upstream parser's treatment of
/// non-version headings such as `[Unreleased]`.
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
    let mut entries = Vec::new();
    let mut current: Option<ChangelogEntry> = None;

    for line in content.lines() {
        if line.starts_with("## ") {
            if let Some(mut entry) = current.take() {
                entry.content = entry.content.trim().to_string();
                if !entry.content.is_empty() {
                    entries.push(entry);
                }
            }
            if let Some((major, minor, patch)) = parse_version_header(line) {
                current = Some(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: line.to_string(),
                });
            }
        } else if let Some(entry) = current.as_mut() {
            entry.content.push('\n');
            entry.content.push_str(line);
        }
    }

    if let Some(mut entry) = current {
        entry.content = entry.content.trim().to_string();
        if !entry.content.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

pub fn compare_versions(left: &ChangelogEntry, right: &ChangelogEntry) -> std::cmp::Ordering {
    (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch))
}

fn parse_version_string(version: &str) -> (u64, u64, u64) {
    let version = version.trim().trim_start_matches('v');
    let mut parts = version.split('.');
    (
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
    )
}

/// Return entries newer than `last_version`, preserving changelog order.
pub fn get_new_entries<'a>(
    entries: &'a [ChangelogEntry],
    last_version: &str,
) -> Vec<&'a ChangelogEntry> {
    let (major, minor, patch) = parse_version_string(last_version);
    entries
        .iter()
        .filter(|entry| (entry.major, entry.minor, entry.patch) > (major, minor, patch))
        .collect()
}

fn split_local_target(target: &str) -> (&str, &str, &str) {
    let (before_hash, fragment) = target.split_once('#').unwrap_or((target, ""));
    let (path, query) = before_hash.split_once('?').unwrap_or((before_hash, ""));
    (path, query, fragment)
}

fn normalize_repository_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let joined = if normalized.starts_with('/') {
        normalized.trim_start_matches('/').to_string()
    } else {
        format!("{CHANGELOG_LINK_BASE_PATH}/{normalized}")
    };
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn is_directory_target(original_path: &str, repository_path: &str) -> bool {
    if original_path.ends_with('/') {
        return true;
    }
    !repository_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .contains('.')
}

fn is_url_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.chars().enumerate().all(|(index, ch)| {
            if index == 0 {
                ch.is_ascii_alphabetic()
            } else {
                ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-')
            }
        })
}

fn encode_uri_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        // Match JavaScript `encodeURI`, including RFC-3986 reserved path
        // characters. Query and fragment delimiters have already been split
        // out by `split_local_target`.
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b';'
                    | b','
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b'/'
            );
        if keep {
            output.push(*byte as char);
        } else {
            output.push('%');
            output.push_str(&format!("{byte:02X}"));
        }
    }
    output
}

fn normalize_link_target(target: &str, tag: &str) -> String {
    // `regex` intentionally does not implement look-around. Spell the
    // boundary out with `strip_prefix` so `pi-mono-extra` is not rewritten
    // while both the repository root and repository-relative paths are.
    let legacy_repositories = [
        "https://github.com/badlogic/pi-mono",
        "https://github.com/earendil-works/pi-mono",
    ];
    let mut canonical = target.to_string();
    for legacy in legacy_repositories {
        if canonical == legacy {
            canonical = format!("https://github.com/{GITHUB_REPO}");
            break;
        }
        if let Some(rest) = canonical.strip_prefix(&format!("{legacy}/")) {
            canonical = format!("https://github.com/{GITHUB_REPO}/{rest}");
            break;
        }
    }
    let repo_url = format!("https://github.com/{GITHUB_REPO}");
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let prefix = format!("{repo_url}/{route}/{branch}/");
            if let Some(rest) = canonical.strip_prefix(&prefix) {
                canonical = format!("{repo_url}/{route}/{tag}/{rest}");
            }
        }
    }

    if canonical.starts_with('#') || canonical.starts_with("//") || is_url_scheme(&canonical) {
        return canonical;
    }

    let (path, query, fragment) = split_local_target(&canonical);
    if path.is_empty() {
        return canonical;
    }
    let trailing_slash = path.ends_with('/');
    let Some(repository_path) = normalize_repository_path(path) else {
        return canonical;
    };
    let route = if is_directory_target(path, &repository_path) {
        "tree"
    } else {
        "blob"
    };
    let mut result = format!(
        "{repo_url}/{route}/{tag}/{}{}",
        encode_uri_path(&repository_path),
        if trailing_slash { "/" } else { "" }
    );
    if !query.is_empty() {
        result.push('?');
        result.push_str(query);
    }
    if !fragment.is_empty() {
        result.push('#');
        result.push_str(fragment);
    }
    result
}

/// Pin package-relative changelog links to the release tag and canonicalize
/// links copied from the old `pi-mono` repository.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    let tag = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    let regex = Regex::new(INLINE_MARKDOWN_LINK_PATTERN).expect("valid changelog link regex");
    regex
        .replace_all(markdown, |captures: &Captures<'_>| {
            format!(
                "{}{}{}",
                &captures[1],
                normalize_link_target(&captures[2], &tag),
                &captures[3]
            )
        })
        .into_owned()
}

pub fn full_markdown(content: &str) -> String {
    let mut entries = parse_changelog(content);
    entries.reverse();
    if entries.is_empty() {
        return "No changelog entries found.".to_string();
    }
    entries
        .iter()
        .map(|entry| normalize_changelog_links(&entry.content, &entry.version()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn new_markdown(content: &str, last_version: &str) -> Option<String> {
    let entries = parse_changelog(content);
    let new_entries = get_new_entries(&entries, last_version);
    if new_entries.is_empty() {
        return None;
    }
    Some(
        new_entries
            .iter()
            .map(|entry| normalize_changelog_links(&entry.content, &entry.version()))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

pub fn read_path(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn embedded_content() -> &'static str {
    include_str!("../../data/CHANGELOG.md")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parser_skips_unreleased_and_preserves_entries() {
        let entries = parse_changelog(
            "# Changelog\n\n## [Unreleased]\n- draft\n\n## [1.2.3] - date\n\n### Added\n- feature\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version(), "1.2.3");
        assert!(entries[0].content.contains("### Added"));
    }

    #[test]
    fn new_entries_compare_semver_components() {
        let entries = parse_changelog("## [1.2.0]\nnew\n\n## [1.1.9]\nold\n");
        let selected = get_new_entries(&entries, "1.1.9");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].version(), "1.2.0");
    }

    #[test]
    fn normalizes_package_and_legacy_links() {
        let markdown = [
            "[Project Trust](README.md#project-trust)",
            "[Extensions](docs/extensions.md#project_trust)",
            "[Examples](examples/extensions/)",
            "[Root README](../../README.md#supply-chain-hardening)",
            "[#5167](https://github.com/earendil-works/pi-mono/pull/5167)",
            "[Agent](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md)",
            "[External](https://example.com/docs)",
        ]
        .join("\n");
        let normalized = normalize_changelog_links(&markdown, "0.79.0");
        assert!(normalized.contains("https://github.com/earendil-works/pi/blob/v0.79.0/packages/coding-agent/README.md#project-trust"));
        assert!(normalized.contains("https://github.com/earendil-works/pi/tree/v0.79.0/packages/coding-agent/examples/extensions/"));
        assert!(normalized.contains("https://github.com/earendil-works/pi/pull/5167"));
        assert!(normalized.contains(
            "https://github.com/earendil-works/pi/blob/v0.79.0/packages/agent/README.md"
        ));
        assert!(normalized.contains("https://example.com/docs"));
    }

    #[test]
    fn matches_upstream_prefix_parsing_and_uri_encoding() {
        let entries = parse_changelog("## 2.3.4-beta\nrelease\n\n## [1.0.0.9] - old\nold\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version(), "2.3.4");
        assert_eq!(entries[1].version(), "1.0.0");

        let normalized = normalize_changelog_links(
            "[comma](docs/a,b.md); [unicode](docs/café.md); [query](README.md?x=1&y=2#top)",
            "1.2.3",
        );
        assert!(normalized.contains("/blob/v1.2.3/packages/coding-agent/docs/a,b.md"));
        assert!(normalized.contains("/blob/v1.2.3/packages/coding-agent/docs/caf%C3%A9.md"));
        assert!(normalized.contains("/blob/v1.2.3/packages/coding-agent/README.md?x=1&y=2#top"));
    }

    #[test]
    fn numeric_colon_targets_are_repository_paths_not_url_schemes() {
        let normalized = normalize_changelog_links("[Guide](123:guide)", "1.2.3");
        assert_eq!(
            normalized,
            "[Guide](https://github.com/earendil-works/pi/tree/v1.2.3/packages/coding-agent/123:guide)"
        );
    }

    #[test]
    fn embedded_catalogue_is_real_upstream_release_asset() {
        let content = embedded_content();
        assert!(content.starts_with("# Changelog"));
        assert!(content.contains("## [0.84.2]"));
        assert!(!full_markdown(content).contains("[Unreleased]"));
    }
}
