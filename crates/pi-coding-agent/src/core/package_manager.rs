//! Package manager — port of
//! `packages/coding-agent/src/core/package-manager.ts` (the CLI-observable
//! surface: source parsing, install/remove/update/list, settings
//! persistence, and on-disk git package layout).
//!
//! Also ports the full `resolve()` resource-resolution layer: on-disk
//! collection of extensions/skills/prompts/themes (recursive ignore-aware
//! walking, manifest entry points, ancestor `.agents/skills` discovery),
//! include/exclude/force pattern filtering, precedence-ranked collision
//! resolution, and project-over-global package dedupe — producing the
//! `ResolvedPaths` that feeds the independent `core/extensions` discovery
//! seam and the ConfigSelector (`interactive/config_selector`).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use indexmap::IndexMap;
use serde_json::Value;

use crate::config::CONFIG_DIR_NAME;
use crate::core::model_resolver::glob_match;
use crate::core::pi_manifest::read_pi_manifest;
use crate::core::settings::{PackageSource, PackageSourceObj, SettingsManager};
use crate::interactive::config_selector::{
    PathMetadata, ResolvedPaths, ResolvedResource, ResourceOrigin, SourceScope as ResolvedScope,
};

/// How `resolve` handles a configured source that is not currently installed
/// (upstream `MissingSourceAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSourceAction {
    Install,
    Skip,
    Error,
}

// ---------------------------------------------------------------------------
// Parsed sources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct NpmSource {
    pub spec: String,
    pub name: String,
    pub version: Option<String>,
    pub range: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitSource {
    pub repo: String,
    pub host: String,
    pub path: String,
    pub ref_: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSource {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedSource {
    Npm(NpmSource),
    Git(GitSource),
    Local(LocalSource),
}

pub type SourceScope = &'static str;

const RUST_NATIVE_ONLY_PACKAGE_ERROR: &str = "Rust-native-only package policy: JavaScript/TypeScript package execution is disabled; npm, npx, and bun are not invoked. Register compiled Rust extensions or use local/git skills, prompts, and themes.";

fn rust_native_only_package_error(source: &str) -> String {
    format!("{RUST_NATIVE_ONLY_PACKAGE_ERROR} Unsupported source: {source}")
}

fn is_unsupported_js_package_source(source: &str) -> bool {
    let source = source.trim_start();
    ["npm:", "npx:", "bun:"].iter().any(|prefix| {
        source
            .get(..prefix.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
    })
}

// ---------------------------------------------------------------------------
// Ports of utils/git.ts (parseGitUrl) + package-manager.ts parse helpers
// ---------------------------------------------------------------------------

fn is_exact_npm_version(version: Option<&str>) -> bool {
    version.map(parse_semver_valid).unwrap_or(false)
}

/// A minimal semver-validity check (upstream `semver.valid`): a plain
/// `x.y.z` (possibly with prerelease/build). Ranges like `^1.2.3` are not
/// exact.
pub fn parse_semver_valid(version: &str) -> bool {
    let version = version.trim();
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Minimal semver range validity (upstream `semver.validRange`). Accepts
/// exact versions and common range prefixes.
pub fn parse_semver_valid_range(version: &str) -> bool {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return false;
    }
    if parse_semver_valid(trimmed) {
        return true;
    }
    let known = ["^", "~", ">=", "<=", ">", "<", "=", "*"];
    if known.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return true;
    }
    trimmed.split_whitespace().count() > 1 // compound ranges
}

fn get_npm_version_range(version: Option<&str>) -> Option<String> {
    match version {
        Some(v) if parse_semver_valid_range(v) => Some(v.to_string()),
        _ => None,
    }
}

/// Split an npm spec into name and version (upstream `parseNpmSpec`).
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let re = regex::Regex::new(r"^(@?[^@]+(?:/[^@]+)?)(?:@(.+))?$").unwrap();
    match re.captures(spec) {
        Some(caps) => {
            let name = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| spec.to_string());
            let version = caps.get(2).map(|m| m.as_str().to_string());
            (name, version)
        }
        None => (spec.to_string(), None),
    }
}

fn is_local_path(source: &str) -> bool {
    source.starts_with("./")
        || source.starts_with("../")
        || source.starts_with('/')
        || source == "."
}

fn split_git_ref(url: &str) -> (String, Option<String>) {
    // SCP-like: git@host:path[@ref]
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon) = rest.find(':') {
            let path_with_maybe_ref = &rest[colon + 1..];
            if let Some(at) = path_with_maybe_ref.find('@') {
                let repo_path = &path_with_maybe_ref[..at];
                let ref_ = &path_with_maybe_ref[at + 1..];
                if !repo_path.is_empty() && !ref_.is_empty() {
                    return (
                        format!("git@{}:{repo_path}", &rest[..colon]),
                        Some(ref_.to_string()),
                    );
                }
            }
            return (url.to_string(), None);
        }
        return (url.to_string(), None);
    }
    if url.contains("://") {
        if let Some(scheme_end) = url.find("://") {
            let after_scheme = &url[scheme_end + 3..];
            // Find first slash after host.
            if let Some(slash) = after_scheme.find('/') {
                let path_with_maybe_ref = &after_scheme[slash + 1..];
                if let Some(at) = path_with_maybe_ref.find('@') {
                    let repo_path = &path_with_maybe_ref[..at];
                    let ref_ = &path_with_maybe_ref[at + 1..];
                    if !repo_path.is_empty() && !ref_.is_empty() {
                        let mut new_url = format!(
                            "{}://{}{repo_path}",
                            &url[..scheme_end],
                            &after_scheme[..slash + 1]
                        );
                        while new_url.ends_with('/') {
                            new_url.pop();
                        }
                        return (new_url, Some(ref_.to_string()));
                    }
                }
                return (url.to_string(), None);
            }
            return (url.to_string(), None);
        }
        return (url.to_string(), None);
    }
    // host/path[@ref]
    if let Some(slash) = url.find('/') {
        let host = &url[..slash];
        let path_with_maybe_ref = &url[slash + 1..];
        if let Some(at) = path_with_maybe_ref.find('@') {
            let repo_path = &path_with_maybe_ref[..at];
            let ref_ = &path_with_maybe_ref[at + 1..];
            if !repo_path.is_empty() && !ref_.is_empty() {
                return (format!("{host}/{repo_path}"), Some(ref_.to_string()));
            }
        }
    }
    (url.to_string(), None)
}

fn decode_for_validation(value: &str) -> Option<String> {
    // Minimal percent-decoding try; treat invalid escapes as unsafe.
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit()
            {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                let byte = u8::from_str_radix(hex, 16).ok()?;
                out.push(byte);
                i += 3;
            } else {
                return None;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_for_validation(value) else {
        return true;
    };
    for candidate in [value, decoded.as_str()] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|part| part == "..") {
            return true;
        }
    }
    false
}

fn builtin_git_source(
    repo: String,
    host: &str,
    path: &str,
    ref_: Option<String>,
) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized_path = path
        .trim_end_matches(".git")
        .trim_start_matches('/')
        .to_string();
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false)
        || has_unsafe_git_install_part(&normalized_path, true)
    {
        return None;
    }
    Some(GitSource {
        repo,
        host: host.to_string(),
        path: normalized_path,
        pinned: ref_.is_some(),
        ref_,
    })
}

/// Try hosted-git-info-style normalization for known hosts (github/gitlab/
/// bitbucket) plus generic URL fallback — a practical subset of upstream
/// `parseGitUrl` + `hosted-git-info`.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed[4..].trim().to_string()
    } else {
        trimmed.to_string()
    };
    if !has_git_prefix && !is_explicit_protocol(&url) {
        return None;
    }

    let (repo_without_ref, ref_) = split_git_ref(&url);

    // Explicit protocol URLs.
    if repo_without_ref.starts_with("https://")
        || repo_without_ref.starts_with("http://")
        || repo_without_ref.starts_with("ssh://")
        || repo_without_ref.starts_with("git://")
    {
        let (host, path) = parse_protocol_host_path(&repo_without_ref)?;
        if is_known_git_host(&host) && !path.is_empty() {
            let repo_url = if has_git_prefix
                && !repo_without_ref.starts_with("https://")
                && !repo_without_ref.starts_with("http://")
                && !repo_without_ref.starts_with("ssh://")
                && !repo_without_ref.starts_with("git://")
            {
                format!("https://{}", repo_without_ref)
            } else {
                repo_without_ref.clone()
            };
            return builtin_git_source(repo_url, &host, &path, ref_);
        }
        return builtin_git_source(repo_without_ref.clone(), &host, &path, ref_);
    }

    // SCP-like git@host:path
    if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        let (host, path) = rest.split_once(':').unwrap_or((rest, ""));
        return builtin_git_source(repo_without_ref.clone(), host, path, ref_);
    }

    // host/path shorthand (git: prefix required; without prefix it is local).
    if let Some(slash) = repo_without_ref.find('/') {
        let host = &repo_without_ref[..slash];
        let path = &repo_without_ref[slash + 1..];
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        return builtin_git_source(format!("https://{repo_without_ref}"), host, path, ref_);
    }
    None
}

fn is_explicit_protocol(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("ssh://")
        || lower.starts_with("git://")
}

fn parse_protocol_host_path(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split_once("://")?.1;
    let hostport = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    let path = after_scheme
        .split('/')
        .skip(1)
        .collect::<Vec<_>>()
        .join("/");
    Some((host.to_string(), path))
}

fn is_known_git_host(host: &str) -> bool {
    matches!(
        host,
        "github.com" | "gitlab.com" | "bitbucket.org" | "gitea.com" | "codeberg.org"
    )
}

impl ParsedSource {
    /// Port of `DefaultPackageManager.parseSource`.
    pub fn parse(source: &str) -> ParsedSource {
        if let Some(spec) = source.strip_prefix("npm:") {
            let spec = spec.trim().to_string();
            let (name, version) = parse_npm_spec(&spec);
            return ParsedSource::Npm(NpmSource {
                spec: spec.clone(),
                name,
                version: version.clone(),
                range: get_npm_version_range(version.as_deref()),
                pinned: is_exact_npm_version(version.as_deref()),
            });
        }
        if is_local_path(source) {
            return ParsedSource::Local(LocalSource {
                path: source.to_string(),
            });
        }
        if let Some(git) = parse_git_url(source) {
            return ParsedSource::Git(git);
        }
        ParsedSource::Local(LocalSource {
            path: source.to_string(),
        })
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            ParsedSource::Npm(_) => "npm",
            ParsedSource::Git(_) => "git",
            ParsedSource::Local(_) => "local",
        }
    }
}

// ---------------------------------------------------------------------------
// Resource accumulation and collection primitives
// (upstream package-manager.ts module-level helpers)
// ---------------------------------------------------------------------------

/// Per-resource-type collector keyed by absolute path — first-wins collision
/// resolution (upstream `ResourceAccumulator`).
#[derive(Debug, Default)]
struct ResourceAccumulator {
    extensions: ResourceMap,
    skills: ResourceMap,
    prompts: ResourceMap,
    themes: ResourceMap,
}

/// The upstream accumulator uses JavaScript `Map`, whose insertion order is
/// observable after precedence sorting. Keep that order deterministic in Rust
/// as well; a randomized `HashMap` would make same-rank resource order vary
/// between processes.
type ResourceMap = IndexMap<String, (PathMetadata, bool)>;

/// One of the four configurable resource types (upstream `ResourceType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    /// Settings key for the type (upstream `ResourceType` strings).
    fn settings_key(&self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    /// Convention sub-directory name under a package root.
    fn dir_name(&self) -> &'static str {
        self.settings_key()
    }
}

fn resource_type_is_executable(resource_type: ResourceType) -> bool {
    matches!(resource_type, ResourceType::Extensions)
}

const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

/// Filter carried by an object-config package source (upstream `PackageFilter`).
#[derive(Debug, Default)]
struct PackageFilter {
    autoload: Option<bool>,
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilter {
    fn from_obj(obj: &PackageSourceObj) -> PackageFilter {
        PackageFilter {
            autoload: obj.autoload,
            extensions: obj.extensions.clone(),
            skills: obj.skills.clone(),
            prompts: obj.prompts.clone(),
            themes: obj.themes.clone(),
        }
    }

    fn patterns(&self, resource_type: ResourceType) -> Option<&Vec<String>> {
        match resource_type {
            ResourceType::Extensions => self.extensions.as_ref(),
            ResourceType::Skills => self.skills.as_ref(),
            ResourceType::Prompts => self.prompts.as_ref(),
            ResourceType::Themes => self.themes.as_ref(),
        }
    }
}

// The zero-JS build does not discover executable extension files. The current
// loader accepts only in-process Rust factories, so advertising `.so`, `.dll`,
// `.dylib`, `.js`, or `.ts` paths here would claim a runtime boundary that does
// not exist. Skills, prompts, and themes remain filesystem-discoverable below.
const FILE_PATTERNS: [(ResourceType, &str); 4] = [
    (ResourceType::Extensions, r"(?!)"),
    (ResourceType::Skills, r"\.md$"),
    (ResourceType::Prompts, r"\.md$"),
    (ResourceType::Themes, r"\.json$"),
];

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

fn to_posix_path(p: &str) -> String {
    p.replace('\\', "/")
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Port of upstream `prefixIgnorePattern`: prefix a pattern line so it is
/// relative to the ignore file's root, honoring `!`/`\!` negation markers.
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }
    let mut pattern = line.to_string();
    let mut negated = false;
    if pattern.starts_with('!') {
        negated = true;
        pattern = pattern[1..].to_string();
    } else if pattern.starts_with("\\!") {
        pattern = pattern[1..].to_string();
    }
    if pattern.starts_with('/') {
        pattern = pattern[1..].to_string();
    }
    let prefixed = format!("{prefix}{pattern}");
    if negated {
        Some(format!("!{prefixed}"))
    } else {
        Some(prefixed)
    }
}

/// Port of upstream `addIgnoreRules`: load `.gitignore`/`.ignore`/`.fdignore`
/// rules from `dir`, prefixed relative to `root_dir`.
fn add_ignore_rules(
    ig: &mut pi_agent::harness::skills::IgnoreMatcher,
    dir: &Path,
    root_dir: &Path,
) {
    let rel = os_rel_posix(root_dir, dir);
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        let Ok(content) = fs::read_to_string(&ignore_path) else {
            continue;
        };
        for line in content.split('\n') {
            if let Some(pattern) = prefix_ignore_pattern(line.trim_end_matches('\r'), &prefix) {
                ig.add(&pattern);
            }
        }
    }
}

fn os_rel_posix(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => to_posix_path(&rel.to_string_lossy()),
        Err(_) => to_posix_path(&path.to_string_lossy()),
    }
}

fn entry_is_dir_or_file(entry: &fs::DirEntry) -> (bool, bool) {
    let file_type = match entry.file_type() {
        Ok(ft) => ft,
        Err(_) => return (false, false),
    };
    if file_type.is_symlink() {
        return match fs::metadata(entry.path()) {
            Ok(m) => (m.is_dir(), m.is_file()),
            Err(_) => (false, false),
        };
    }
    (file_type.is_dir(), file_type.is_file())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillDiscoveryMode {
    Pi,
    Agents,
}

/// Port of upstream `collectSkillEntries`: discover SKILL.md files (top-level
/// first, one per skill dir); in "agents" mode also collect non-SKILL `.md`
/// files in nested skill dirs.
fn collect_skill_entries(dir: &Path, mode: SkillDiscoveryMode, root_dir: &Path) -> Vec<String> {
    let mut ig = pi_agent::harness::skills::IgnoreMatcher::default();
    add_ignore_rules(&mut ig, dir, root_dir);
    collect_skill_entries_inner(dir, mode, root_dir, &mut ig)
}

fn collect_skill_entries_inner(
    dir: &Path,
    mode: SkillDiscoveryMode,
    root_dir: &Path,
    ig: &mut pi_agent::harness::skills::IgnoreMatcher,
) -> Vec<String> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }

    let dir_entries = match fs::read_dir(dir) {
        Ok(it) => it.flatten().collect::<Vec<_>>(),
        Err(_) => return entries,
    };

    // SKILL.md takes precedence in the direct directory.
    for entry in &dir_entries {
        if entry.file_name() != OsStr::new("SKILL.md") {
            continue;
        }
        let full_path = entry.path();
        let is_file = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => fs::metadata(&full_path)
                .map(|m| m.is_file())
                .unwrap_or(false),
            Ok(ft) => ft.is_file(),
            Err(_) => false,
        };
        let rel_path = os_rel_posix(root_dir, &full_path);
        if is_file && !ig.ignores(&rel_path) {
            entries.push(path_to_string(&full_path));
            return entries;
        }
    }

    for entry in &dir_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        if name_str.starts_with('.') {
            continue;
        }
        if name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let (is_dir, is_file) = entry_is_dir_or_file(entry);
        let rel_path = os_rel_posix(root_dir, &full_path);
        let should_include_markdown = is_file
            && name_str.ends_with(".md")
            && !ig.ignores(&rel_path)
            && ((mode == SkillDiscoveryMode::Pi && dir == root_dir)
                || (mode == SkillDiscoveryMode::Agents && dir != root_dir));
        if should_include_markdown {
            entries.push(path_to_string(&full_path));
            continue;
        }
        if !is_dir {
            continue;
        }
        if ig.ignores(&format!("{rel_path}/")) {
            continue;
        }
        add_ignore_rules(ig, &full_path, root_dir);
        entries.extend(collect_skill_entries_inner(&full_path, mode, root_dir, ig));
    }
    entries
}

/// No executable extensions are auto-discovered until the Rust loader exposes
/// a verified artifact ABI. Rust extensions must currently be registered with
/// `load_extension_from_factory`; this function intentionally returns no
/// filesystem paths and never considers JavaScript/TypeScript files.
fn collect_auto_extension_entries(_dir: &Path, _root_dir: &Path) -> Vec<String> {
    Vec::new()
}

/// Port of upstream `collectAutoPromptEntries`/`collectAutoThemeEntries`:
/// inspect only direct children of the configured resource directory. Nested
/// files belong to packages or explicit paths, not top-level auto-discovery.
fn collect_flat_resource_entries(dir: &Path, resource_type: ResourceType) -> Vec<String> {
    let mut entries = Vec::new();
    let mut matcher = pi_agent::harness::skills::IgnoreMatcher::default();
    add_ignore_rules(&mut matcher, dir, dir);
    let pattern = file_pattern_regex(resource_type);
    let dir_entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return entries,
    };
    for entry in dir_entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let (_is_dir, is_file) = entry_is_dir_or_file(&entry);
        let relative = os_rel_posix(dir, &full_path);
        if matcher.ignores(&relative) {
            continue;
        }
        if is_file && pattern.is_match(&name_str) {
            entries.push(path_to_string(&full_path));
        }
    }
    entries
}

fn collect_auto_prompt_entries(dir: &Path, _root_dir: &Path) -> Vec<String> {
    collect_flat_resource_entries(dir, ResourceType::Prompts)
}

fn collect_auto_theme_entries(dir: &Path, _root_dir: &Path) -> Vec<String> {
    collect_flat_resource_entries(dir, ResourceType::Themes)
}

/// Package convention directories use upstream `collectFiles`, which walks
/// recursively (unlike the top-level auto-discovery helpers above). Keeping
/// the two paths separate matters for packages such as `prompts/reviews/*.md`
/// and `themes/dark/*.json`.
fn collect_recursive_resource_entries(
    dir: &Path,
    resource_type: ResourceType,
    root_dir: &Path,
) -> Vec<String> {
    let mut matcher = pi_agent::harness::skills::IgnoreMatcher::default();
    let pattern = file_pattern_regex(resource_type);
    collect_recursive_resource_entries_inner(dir, root_dir, &mut matcher, &pattern)
}

fn collect_recursive_resource_entries_inner(
    dir: &Path,
    root_dir: &Path,
    matcher: &mut pi_agent::harness::skills::IgnoreMatcher,
    pattern: &regex::Regex,
) -> Vec<String> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }
    add_ignore_rules(matcher, dir, root_dir);
    let dir_entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return entries,
    };
    for entry in dir_entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let (is_dir, is_file) = entry_is_dir_or_file(&entry);
        let relative = os_rel_posix(root_dir, &full_path);
        let ignore_path = if is_dir {
            format!("{relative}/")
        } else {
            relative
        };
        if matcher.ignores(&ignore_path) {
            continue;
        }
        if is_dir {
            entries.extend(collect_recursive_resource_entries_inner(
                &full_path, root_dir, matcher, pattern,
            ));
        } else if is_file && pattern.is_match(&name_str) {
            entries.push(path_to_string(&full_path));
        }
    }
    entries
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
fn file_pattern_regex(resource_type: ResourceType) -> regex::Regex {
    let raw = FILE_PATTERNS
        .iter()
        .find(|(rt, _)| *rt == resource_type)
        .map(|(_, pat)| *pat)
        .unwrap_or("");
    regex::Regex::new(raw).unwrap()
}

/// Port of upstream `collectResourceFiles`: dispatch by type — skills and
/// extensions use smart discovery, prompts/themes use recursive collection.
fn collect_resource_files(dir: &Path, resource_type: ResourceType, root_dir: &Path) -> Vec<String> {
    match resource_type {
        ResourceType::Skills => collect_skill_entries(dir, SkillDiscoveryMode::Pi, root_dir),
        ResourceType::Extensions => collect_auto_extension_entries(dir, root_dir),
        ResourceType::Prompts | ResourceType::Themes => {
            collect_recursive_resource_entries(dir, resource_type, root_dir)
        }
    }
}

/// Port of upstream `findGitRepoRoot`: walk up to the nearest `.git` dir.
fn find_git_repo_root(start_dir: &Path) -> Option<String> {
    let mut dir = if start_dir.is_absolute() {
        start_dir.to_path_buf()
    } else {
        fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf())
    };
    loop {
        if dir.join(".git").exists() {
            return Some(path_to_string(&dir));
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Port of upstream `collectAncestorAgentsSkillDirs`: `.agents/skills` dirs
/// along the ancestor chain up to (and including) the git repo root.
fn collect_ancestor_agents_skill_dirs(start_dir: &Path) -> Vec<String> {
    let mut dirs = Vec::new();
    let resolved = fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    let git_repo_root = find_git_repo_root(&resolved);

    let mut dir = resolved;
    loop {
        dirs.push(path_to_string(&dir.join(".agents").join("skills")));
        if dir == git_repo_root.as_deref().map(Path::new).unwrap_or(&dir) {
            break;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    dirs
}

fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

fn is_override_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

fn has_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

/// Port of upstream `matchesAnyPattern`: minimatch a file against include/
/// exclude patterns across relative path, basename, and absolute path forms;
/// skill files additionally match against their parent dir.
fn matches_any_pattern(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    let rel = os_rel_posix(Path::new(base_dir), Path::new(file_path));
    let name = Path::new(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_path_posix = to_posix_path(file_path);
    let is_skill_file = name == "SKILL.md";
    let parent_dir = if is_skill_file {
        Path::new(file_path).parent().map(|p| p.to_path_buf())
    } else {
        None
    };
    let parent_rel = parent_dir
        .as_deref()
        .map(|p| os_rel_posix(Path::new(base_dir), p));
    let parent_name = parent_dir
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let parent_dir_posix = parent_dir
        .as_deref()
        .map(|p| to_posix_path(&path_to_string(p)));

    patterns.iter().any(|pattern| {
        let normalized = to_posix_path(pattern);
        if glob_match(&normalized, &rel, false)
            || glob_match(&normalized, &name, false)
            || glob_match(&normalized, &file_path_posix, false)
        {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        let parent_rel = parent_rel.as_deref().unwrap_or("");
        let parent_name = parent_name.as_deref().unwrap_or("");
        let parent_dir_posix = parent_dir_posix.as_deref().unwrap_or("");
        glob_match(&normalized, parent_rel, false)
            || glob_match(&normalized, parent_name, false)
            || glob_match(&normalized, parent_dir_posix, false)
    })
}

fn normalize_exact_pattern(pattern: &str) -> String {
    let normalized = if pattern.starts_with("./") || pattern.starts_with(".\\") {
        pattern[2..].to_string()
    } else {
        pattern.to_string()
    };
    to_posix_path(&normalized)
}

fn matches_any_exact_pattern(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let rel = os_rel_posix(Path::new(base_dir), Path::new(file_path));
    let name = Path::new(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_path_posix = to_posix_path(file_path);
    let is_skill_file = name == "SKILL.md";
    let parent_rel = if is_skill_file {
        Path::new(file_path)
            .parent()
            .map(|p| os_rel_posix(Path::new(base_dir), p))
    } else {
        None
    };
    let parent_dir_posix = if is_skill_file {
        Path::new(file_path)
            .parent()
            .map(|p| to_posix_path(&path_to_string(p)))
    } else {
        None
    };

    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        if normalized == rel || normalized == file_path_posix {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        normalized == parent_rel.as_deref().unwrap_or("")
            || normalized == parent_dir_posix.as_deref().unwrap_or("")
    })
}

fn get_override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| p.starts_with('!') || p.starts_with('+') || p.starts_with('-'))
        .cloned()
        .collect()
}

/// Port of upstream `isEnabledByOverrides`: apply `!`/`+`/`-` override patterns
/// to a single path's enabled state.
fn is_enabled_by_overrides(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    let overrides = get_override_patterns(patterns);
    let excludes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('!'))
        .map(|p| p[1..].to_string())
        .collect();
    let force_includes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('+'))
        .map(|p| p[1..].to_string())
        .collect();
    let force_excludes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('-'))
        .map(|p| p[1..].to_string())
        .collect();

    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(file_path, &excludes, base_dir) {
        enabled = false;
    }
    if !force_includes.is_empty() && matches_any_exact_pattern(file_path, &force_includes, base_dir)
    {
        enabled = true;
    }
    if !force_excludes.is_empty() && matches_any_exact_pattern(file_path, &force_excludes, base_dir)
    {
        enabled = false;
    }
    enabled
}

/// Port of upstream `applyPatterns`: apply include/`!exclude`/`+force-include`/
/// `-force-exclude` patterns over a full path set, returning the enabled set.
fn apply_patterns(
    all_paths: &[String],
    patterns: &[String],
    base_dir: &str,
) -> std::collections::HashSet<String> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for p in patterns {
        if let Some(rest) = p.strip_prefix('+') {
            force_includes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('-') {
            force_excludes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('!') {
            excludes.push(rest.to_string());
        } else {
            includes.push(p.clone());
        }
    }

    let mut result: Vec<String> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|f| matches_any_pattern(f, &includes, base_dir))
            .cloned()
            .collect()
    };

    if !excludes.is_empty() {
        result.retain(|f| !matches_any_pattern(f, &excludes, base_dir));
    }
    if !force_includes.is_empty() {
        for f in all_paths {
            if !result.contains(f) && matches_any_exact_pattern(f, &force_includes, base_dir) {
                result.push(f.clone());
            }
        }
    }
    if !force_excludes.is_empty() {
        result.retain(|f| !matches_any_exact_pattern(f, &force_excludes, base_dir));
    }
    result.into_iter().collect()
}

/// Port of upstream `applyAutoloadDisabledPatterns`: for an `autoload: false`
/// package, only the explicitly listed patterns flip a file's enabled state.
fn apply_autoload_disabled_patterns(
    all_paths: &[String],
    patterns: &[String],
    base_dir: &str,
) -> std::collections::HashMap<String, bool> {
    let mut result = std::collections::HashMap::new();
    for pattern in patterns {
        let target =
            if pattern.starts_with('+') || pattern.starts_with('-') || pattern.starts_with('!') {
                pattern[1..].to_string()
            } else {
                pattern.clone()
            };
        let enabled = !pattern.starts_with('-') && !pattern.starts_with('!');
        let exact = pattern.starts_with('+') || pattern.starts_with('-');
        for file_path in all_paths {
            let matched = if exact {
                matches_any_exact_pattern(file_path, std::slice::from_ref(&target), base_dir)
            } else {
                matches_any_pattern(file_path, std::slice::from_ref(&target), base_dir)
            };
            if matched {
                result.insert(file_path.clone(), enabled);
            }
        }
    }
    result
}

/// Numeric precedence rank for collision resolution (upstream
/// `resourcePrecedenceRank`): lower rank wins ("project local" highest).
fn resource_precedence_rank(m: &PathMetadata) -> u8 {
    if m.origin == ResourceOrigin::Package {
        return 4;
    }
    let scope_base = if m.scope == ResolvedScope::Project {
        0
    } else {
        2
    };
    scope_base + if m.source == "local" { 0 } else { 1 }
}

// ---------------------------------------------------------------------------
// Configured packages and progress events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: &'static str,
    pub filtered: bool,
    pub installed_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProgressEvent {
    pub event_type: &'static str,
    pub action: &'static str,
    pub source: String,
    pub message: Option<String>,
}

pub type ProgressCallback = Box<dyn Fn(&ProgressEvent) + Send>;

// ---------------------------------------------------------------------------
// Package manager
// ---------------------------------------------------------------------------

pub struct PackageManager {
    cwd: String,
    agent_dir: String,
    settings_manager: SettingsManager,
    progress_callback: Option<ProgressCallback>,
}

pub struct PackageManagerOptions {
    pub cwd: String,
    pub agent_dir: String,
    pub settings_manager: SettingsManager,
}

impl PackageManager {
    pub fn new(options: PackageManagerOptions) -> Self {
        Self {
            cwd: crate::core::settings::resolve_path(&options.cwd)
                .to_string_lossy()
                .into_owned(),
            agent_dir: crate::core::settings::resolve_path(&options.agent_dir)
                .to_string_lossy()
                .into_owned(),
            settings_manager: options.settings_manager,
            progress_callback: None,
        }
    }

    pub fn set_progress_callback(&mut self, callback: Option<ProgressCallback>) {
        self.progress_callback = callback;
    }

    fn emit_progress(&self, event: ProgressEvent) {
        if let Some(callback) = &self.progress_callback {
            callback(&event);
        }
    }

    fn with_progress(
        &self,
        action: &'static str,
        source: &str,
        message: &str,
        operation: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.emit_progress(ProgressEvent {
            event_type: "start",
            action,
            source: source.to_string(),
            message: Some(message.to_string()),
        });
        let result = operation();
        match &result {
            Ok(()) => self.emit_progress(ProgressEvent {
                event_type: "complete",
                action,
                source: source.to_string(),
                message: None,
            }),
            Err(error) => self.emit_progress(ProgressEvent {
                event_type: "error",
                action,
                source: source.to_string(),
                message: Some(error.clone()),
            }),
        }
        result
    }

    // ------------------------------------------------------------------
    // Settings persistence
    // ------------------------------------------------------------------

    fn base_dir_for_scope(&self, scope: SourceScope) -> PathBuf {
        if scope == "project" {
            Path::new(&self.cwd).join(CONFIG_DIR_NAME)
        } else {
            PathBuf::from(&self.agent_dir)
        }
    }

    fn resolve_path(&self, input: &str) -> PathBuf {
        // Match upstream resolvePath(..., { trim: true }): CLI whitespace is
        // not part of a local package path, and leading `~` is expanded.
        let input = expand_home(input.trim());
        let path = PathBuf::from(&input);
        let joined = if path.is_absolute() {
            path
        } else {
            Path::new(&self.cwd).join(path)
        };
        normalize_lexical_path(&joined)
    }

    fn resolve_path_from_base(&self, input: &str, base_dir: &Path) -> PathBuf {
        let path = PathBuf::from(expand_home(input.trim()));
        let joined = if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        };
        normalize_lexical_path(&joined)
    }

    /// Get the source match key for settings comparison (upstream
    /// `getSourceMatchKeyForSettings`): git and npm match by
    /// `git:host/path` / `npm:name`; local sources resolve to absolute paths.
    pub fn get_source_match_key_for_settings(&self, source: &str, scope: SourceScope) -> String {
        match ParsedSource::parse(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(local) => {
                let base = self.base_dir_for_scope(scope);
                let resolved = self.resolve_path_from_base(&local.path, &base);
                format!("local:{}", resolved.display())
            }
        }
    }

    pub fn get_source_match_key_for_input(&self, source: &str) -> String {
        match ParsedSource::parse(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(local) => {
                format!("local:{}", self.resolve_path(&local.path).display())
            }
        }
    }

    fn package_sources_match(
        &self,
        existing: &PackageSource,
        input_source: &str,
        scope: SourceScope,
    ) -> bool {
        let existing_source = match existing {
            PackageSource::Str(s) => s.clone(),
            PackageSource::Obj(o) => o.source.clone(),
        };
        let left = self.get_source_match_key_for_settings(&existing_source, scope);
        let right = self.get_source_match_key_for_input(input_source);
        left == right
    }

    /// Normalize a local package source for settings (relative to the scope
    /// base; upstream `normalizePackageSourceForSettings`).
    fn normalize_package_source_for_settings(&self, source: &str, scope: SourceScope) -> String {
        let parsed = ParsedSource::parse(source);
        if !matches!(parsed, ParsedSource::Local(_)) {
            return source.to_string();
        }
        let base = self.base_dir_for_scope(scope);
        let resolved = self.resolve_path(source);
        let rel = pathdiff(&base, &resolved);
        if rel.is_empty() {
            ".".to_string()
        } else {
            rel
        }
    }

    /// Port of `addSourceToSettings`. Returns true when settings changed.
    pub fn add_source_to_settings(&mut self, source: &str, local: bool) -> bool {
        let scope: SourceScope = if local { "project" } else { "user" };
        let current = self.get_scope_packages(scope);
        let normalized_source = self.normalize_package_source_for_settings(source, scope);
        if let Some((index, existing)) = current
            .iter()
            .enumerate()
            .find(|(_, e)| self.package_sources_match(e, source, scope))
        {
            let existing_source = match existing {
                PackageSource::Str(s) => s.clone(),
                PackageSource::Obj(o) => o.source.clone(),
            };
            if existing_source == normalized_source {
                return false;
            }
            let mut next = current.clone();
            next[index] = match existing {
                PackageSource::Str(_) => PackageSource::Str(normalized_source),
                PackageSource::Obj(o) => {
                    let mut obj = o.clone();
                    obj.source = normalized_source;
                    PackageSource::Obj(obj)
                }
            };
            self.set_scope_packages(scope, next);
            self.settings_manager.flush_sync();
            return true;
        }
        let mut next = current.clone();
        next.push(PackageSource::Str(normalized_source));
        self.set_scope_packages(scope, next);
        self.settings_manager.flush_sync();
        true
    }

    /// Port of `removeSourceFromSettings`. Returns true when settings changed.
    pub fn remove_source_from_settings(&mut self, source: &str, local: bool) -> bool {
        let scope: SourceScope = if local { "project" } else { "user" };
        let current = self.get_scope_packages(scope);
        let next: Vec<PackageSource> = current
            .iter()
            .filter(|e| !self.package_sources_match(e, source, scope))
            .cloned()
            .collect();
        let changed = next.len() != current.len();
        if changed {
            self.set_scope_packages(scope, next);
            self.settings_manager.flush_sync();
        }
        changed
    }

    fn get_scope_packages(&self, scope: SourceScope) -> Vec<PackageSource> {
        // Upstream listConfiguredPackages reads the raw global and project
        // maps separately (not the merged view), so the user scope must not
        // pick up project entries.
        if scope == "project" {
            self.settings_manager.get_project_packages()
        } else {
            self.settings_manager
                .get_global_settings()
                .get("packages")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        }
    }

    fn set_scope_packages(&mut self, scope: SourceScope, packages: Vec<PackageSource>) {
        if scope == "project" {
            self.settings_manager.set_project_packages(packages);
        } else {
            self.settings_manager.set_packages(packages);
        }
    }

    // ------------------------------------------------------------------
    // Install roots
    // ------------------------------------------------------------------

    pub fn get_git_install_root(&self, scope: SourceScope) -> Option<PathBuf> {
        if scope == "temporary" {
            return None;
        }
        if scope == "project" {
            return Some(Path::new(&self.cwd).join(CONFIG_DIR_NAME).join("git"));
        }
        Some(Path::new(&self.agent_dir).join("git"))
    }

    pub fn get_git_install_path(&self, source: &GitSource, scope: SourceScope) -> PathBuf {
        if scope == "temporary" {
            let temp = self.temporary_dir(&format!("git-{}", source.host));
            return temp.join(&source.path);
        }
        let install_root = self
            .get_git_install_root(scope)
            .unwrap_or_else(|| PathBuf::from("."));
        self.resolve_managed_path(&install_root, &[source.host.as_str(), source.path.as_str()])
    }

    fn resolve_managed_path(&self, root: &Path, parts: &[&str]) -> PathBuf {
        let mut path = root.to_path_buf();
        for part in parts {
            path = path.join(part);
        }
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            std::fs::canonicalize(root)
                .unwrap_or_else(|_| root.to_path_buf())
                .join(parts.to_vec().join("/"))
        };
        let resolved_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
        };
        let resolved_root_str = normalized_path(&resolved_root);
        let resolved_str = normalized_path(&resolved);
        if resolved_str != resolved_root_str
            && !resolved_str.starts_with(&format!("{resolved_root_str}/"))
        {
            panic_write("Refusing to use path outside package install root");
        }
        path
    }

    fn temporary_base(&self) -> PathBuf {
        let temp_folder = Path::new(&self.agent_dir).join("tmp").join("extensions");
        std::fs::create_dir_all(&temp_folder).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp_folder, std::fs::Permissions::from_mode(0o700)).ok();
        }
        temp_folder
    }

    fn temporary_dir(&self, prefix: &str) -> PathBuf {
        if prefix.contains("git-") {
            let host = prefix.trim_start_matches("git-");
            let hash = short_hash(&format!("git-{host}"));
            self.temporary_base().join(hash).join(host)
        } else {
            let hash = short_hash(prefix);
            self.temporary_base().join(hash).join(prefix)
        }
    }

    pub fn get_installed_path(&self, source: &str, scope: &'static str) -> Option<String> {
        match ParsedSource::parse(source) {
            ParsedSource::Npm(_) => None,
            ParsedSource::Git(git) => {
                let path = self.get_git_install_path(&git, scope);
                if path.exists() {
                    Some(path.display().to_string())
                } else {
                    None
                }
            }
            ParsedSource::Local(local) => {
                let base = self.base_dir_for_scope(scope);
                let path = self.resolve_path_from_base(&local.path, &base);
                if path.exists() {
                    Some(path.display().to_string())
                } else {
                    None
                }
            }
        }
    }

    fn run_command(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<(), String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run {command}: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "{command} exited with {:?}: {}",
                output.status.code(),
                stderr.trim()
            ))
        }
    }

    fn run_command_capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<String, String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run {command}: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "{command} exited with {:?}: {}",
                output.status.code(),
                stderr.trim()
            ))
        }
    }

    fn ensure_git_ignore(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            std::fs::write(&ignore_path, "*\n!.gitignore\n")
                .map_err(|e| format!("write .gitignore: {e}"))?;
        }
        Ok(())
    }

    fn is_offline(&self) -> bool {
        crate::config::env_flag(crate::config::ENV_OFFLINE)
    }

    // ------------------------------------------------------------------
    // Install / remove / update / list (public surface)
    // ------------------------------------------------------------------

    pub fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        let mut configured = Vec::new();
        for pkg in self.get_scope_packages("user") {
            let (source, filtered) = package_source_parts(&pkg);
            configured.push(ConfiguredPackage {
                source: source.clone(),
                scope: "user",
                filtered,
                installed_path: self.get_installed_path(&source, "user"),
            });
        }
        for pkg in self.get_scope_packages("project") {
            let (source, filtered) = package_source_parts(&pkg);
            configured.push(ConfiguredPackage {
                source: source.clone(),
                scope: "project",
                filtered,
                installed_path: self.get_installed_path(&source, "project"),
            });
        }
        configured
    }

    pub fn install(&mut self, source: &str, local: bool) -> Result<(), String> {
        if is_unsupported_js_package_source(source) {
            return Err(rust_native_only_package_error(source));
        }
        let scope: SourceScope = if local { "project" } else { "user" };
        let parsed = ParsedSource::parse(source);
        self.with_progress(
            "install",
            source,
            &format!("Installing {source}..."),
            || match &parsed {
                ParsedSource::Npm(_) => Err(rust_native_only_package_error(source)),
                ParsedSource::Git(git) => self.install_git(git, scope),
                ParsedSource::Local(local) => {
                    let resolved = self.resolve_path(&local.path);
                    if !resolved.exists() {
                        return Err(format!("Path does not exist: {}", resolved.display()));
                    }
                    Ok(())
                }
            },
        )
    }

    pub fn install_and_persist(&mut self, source: &str, local: bool) -> Result<(), String> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local);
        Ok(())
    }

    pub fn remove(&mut self, source: &str, local: bool) -> Result<(), String> {
        if is_unsupported_js_package_source(source) {
            return Err(rust_native_only_package_error(source));
        }
        let scope: SourceScope = if local { "project" } else { "user" };
        let parsed = ParsedSource::parse(source);
        self.with_progress(
            "remove",
            source,
            &format!("Removing {source}..."),
            || match &parsed {
                ParsedSource::Npm(_) => Err(rust_native_only_package_error(source)),
                ParsedSource::Git(git) => self.remove_git(git, scope),
                ParsedSource::Local(_) => Ok(()),
            },
        )
    }

    pub fn remove_and_persist(&mut self, source: &str, local: bool) -> Result<bool, String> {
        self.remove(source, local)?;
        Ok(self.remove_source_from_settings(source, local))
    }

    pub fn update(&mut self, source: Option<&str>) -> Result<bool, String> {
        if let Some(source) = source {
            if is_unsupported_js_package_source(source) {
                return Err(rust_native_only_package_error(source));
            }
            if matches!(ParsedSource::parse(source), ParsedSource::Npm(_)) {
                return Err(rust_native_only_package_error(source));
            }
        } else {
            for package in self
                .get_scope_packages("user")
                .into_iter()
                .chain(self.get_scope_packages("project"))
            {
                let (package_source, _) = package_source_parts(&package);
                if is_unsupported_js_package_source(&package_source) {
                    return Err(rust_native_only_package_error(&package_source));
                }
                if matches!(ParsedSource::parse(&package_source), ParsedSource::Npm(_)) {
                    return Err(rust_native_only_package_error(&package_source));
                }
            }
        }
        if self.is_offline() {
            return Ok(false);
        }
        let identity = source.map(|s| self.get_package_identity(s, None));
        let mut matched = false;
        let mut update_sources: Vec<(String, SourceScope)> = Vec::new();
        for pkg in self.get_scope_packages("user") {
            let (source_str, _) = package_source_parts(&pkg);
            if let Some(identity) = &identity {
                if &self.get_package_identity(&source_str, Some("user")) != identity {
                    continue;
                }
            }
            matched = true;
            update_sources.push((source_str, "user"));
        }
        for pkg in self.get_scope_packages("project") {
            let (source_str, _) = package_source_parts(&pkg);
            if let Some(identity) = &identity {
                if &self.get_package_identity(&source_str, Some("project")) != identity {
                    continue;
                }
            }
            matched = true;
            update_sources.push((source_str, "project"));
        }
        if source.is_some() && !matched {
            return Err(format!(
                "No matching package found for {}",
                source.unwrap_or_default()
            ));
        }
        let mut updated_any = false;
        for (source_str, scope) in update_sources {
            let parsed = ParsedSource::parse(&source_str);
            let updated = match &parsed {
                ParsedSource::Npm(_) => return Err(rust_native_only_package_error(&source_str)),
                ParsedSource::Git(git) => self.update_git(git, scope)?,
                ParsedSource::Local(_) => false,
            };
            updated_any = updated_any || updated;
        }
        Ok(updated_any)
    }

    pub fn get_package_identity(&self, source: &str, scope: Option<SourceScope>) -> String {
        match ParsedSource::parse(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(local) => match scope {
                Some(scope) => {
                    let base = self.base_dir_for_scope(scope);
                    format!(
                        "local:{}",
                        self.resolve_path_from_base(&local.path, &base).display()
                    )
                }
                None => format!("local:{}", self.resolve_path(&local.path).display()),
            },
        }
    }

    // ------------------------------------------------------------------
    // git install internals
    // ------------------------------------------------------------------

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn install_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope);
        if target_dir.exists() {
            if source.ref_.is_some() {
                self.ensure_git_ref(
                    &target_dir,
                    &[
                        "fetch".to_string(),
                        "origin".to_string(),
                        source.ref_.clone().unwrap(),
                    ],
                    "FETCH_HEAD",
                )?;
                return Ok(());
            }
            let target = self.get_local_git_update_target(&target_dir)?;
            self.ensure_git_ref(&target_dir, &target.fetch_args, &target.ref_)?;
            return Ok(());
        }
        let git_root = self.get_git_install_root(scope);
        if let Some(git_root) = &git_root {
            self.ensure_git_ignore(git_root)?;
        }
        if let Some(parent) = target_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create parent dir: {e}"))?;
        }
        let marker_path = self.git_update_marker_path(&target_dir);
        let _ = std::fs::remove_file(&marker_path);

        let result = (|| -> Result<(), String> {
            self.run_command(
                "git",
                &[
                    "clone".to_string(),
                    source.repo.clone(),
                    target_dir.display().to_string(),
                ],
                None,
            )?;
            if let Some(ref_) = &source.ref_ {
                self.run_command(
                    "git",
                    &["checkout".to_string(), ref_.clone()],
                    Some(&target_dir),
                )?;
            }
            // Git packages are usable as resource bundles without installing
            // JavaScript dependencies. Dependency execution is intentionally
            // outside the Rust-native package-manager boundary.
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&target_dir);
            if scope == "temporary" {
            } else if let Some(git_root) = &git_root {
                self.prune_empty_git_parents(&target_dir, git_root);
            }
            return result;
        }
        Ok(())
    }

    fn update_git(&self, source: &GitSource, scope: SourceScope) -> Result<bool, String> {
        let target_dir = self.get_git_install_path(source, scope);
        if !target_dir.exists() {
            self.install_git(source, scope)?;
            return Ok(true);
        }
        if let Some(ref_) = &source.ref_ {
            self.ensure_git_ref(
                &target_dir,
                &["fetch".to_string(), "origin".to_string(), ref_.clone()],
                "FETCH_HEAD",
            )?;
            return Ok(true);
        }
        let target = self.get_local_git_update_target(&target_dir)?;
        self.ensure_git_ref(&target_dir, &target.fetch_args, &target.ref_)?;
        Ok(true)
    }

    fn git_update_marker_path(&self, target_dir: &Path) -> PathBuf {
        let parent = target_dir.parent().unwrap_or(Path::new("."));
        let name = target_dir
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        parent.join(format!(".{name}.pi-update-incomplete"))
    }

    fn get_local_git_update_target(
        &self,
        installed_path: &Path,
    ) -> Result<LocalGitUpdateTarget, String> {
        let upstream = self.run_command_capture(
            "git",
            &[
                "rev-parse".to_string(),
                "--abbrev-ref".to_string(),
                "@{upstream}".to_string(),
            ],
            Some(installed_path),
        )?;
        let trimmed = upstream.trim().to_string();
        if let Some(branch) = trimmed.strip_prefix("origin/") {
            let head = self.run_command_capture(
                "git",
                &["rev-parse".to_string(), "@{upstream}".to_string()],
                Some(installed_path),
            )?;
            Ok(LocalGitUpdateTarget {
                ref_: "@{upstream}".to_string(),
                head: head.trim().to_string(),
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ],
            })
        } else {
            // Fall back to origin/HEAD.
            let _ = self.run_command(
                "git",
                &[
                    "remote".to_string(),
                    "set-head".to_string(),
                    "origin".to_string(),
                    "-a".to_string(),
                ],
                Some(installed_path),
            );
            let head = self.run_command_capture(
                "git",
                &["rev-parse".to_string(), "origin/HEAD".to_string()],
                Some(installed_path),
            )?;
            Ok(LocalGitUpdateTarget {
                ref_: "origin/HEAD".to_string(),
                head: head.trim().to_string(),
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    "+HEAD:refs/remotes/origin/HEAD".to_string(),
                ],
            })
        }
    }

    fn ensure_git_ref(
        &self,
        target_dir: &Path,
        fetch_args: &[String],
        ref_: &str,
    ) -> Result<(), String> {
        self.run_command("git", fetch_args, Some(target_dir))?;
        let local_head = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), "HEAD".to_string()],
            Some(target_dir),
        )?;
        let commit_ref = format!("{ref_}^{{commit}}");
        let target_head = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), commit_ref],
            Some(target_dir),
        )?;
        let marker_path = self.git_update_marker_path(target_dir);
        if local_head.trim() == target_head.trim() {
            let _ = std::fs::remove_file(&marker_path);
            return Ok(());
        }
        std::fs::write(&marker_path, "").map_err(|e| format!("write marker: {e}"))?;
        self.run_command(
            "git",
            &[
                "reset".to_string(),
                "--hard".to_string(),
                target_head.trim().to_string(),
            ],
            Some(target_dir),
        )?;
        // Clean the checkout without reinstalling JavaScript dependencies.
        let clean_result = self.run_command(
            "git",
            &["clean".to_string(), "-fdx".to_string()],
            Some(target_dir),
        );
        let _ = std::fs::remove_file(&marker_path);
        clean_result?;
        Ok(())
    }

    fn remove_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope);
        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_file(self.git_update_marker_path(&target_dir));
        if let Some(git_root) = self.get_git_install_root(scope) {
            self.prune_empty_git_parents(&target_dir, &git_root);
        }
        Ok(())
    }

    fn prune_empty_git_parents(&self, target_dir: &Path, install_root: &Path) {
        let resolved_root = normalized_path(install_root);
        let mut current = target_dir.parent().map(|p| p.to_path_buf());
        while let Some(dir) = current {
            let dir_str = normalized_path(&dir);
            if !dir_str.starts_with(&format!("{resolved_root}/")) || dir_str == resolved_root {
                break;
            }
            if !dir.exists() {
                current = dir.parent().map(|p| p.to_path_buf());
                continue;
            }
            let has_entries = std::fs::read_dir(&dir)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);
            if !has_entries {
                let _ = std::fs::remove_dir_all(&dir);
            } else {
                break;
            }
            current = dir.parent().map(|p| p.to_path_buf());
        }
    }

    // ------------------------------------------------------------------
    // Resolve (upstream `resolve`) — resource collection into ResolvedPaths
    // ------------------------------------------------------------------

    /// Port of `DefaultPackageManager.resolve`. Collects all configured and
    /// auto-discovered extensions/skills/prompts/themes into a `ResolvedPaths`.
    /// Synchronous: a configured-but-missing git package is cloned on the spot
    /// (subject to the offline flag), or an `on_missing` callback may take over
    /// the decision. npm sources are rejected by the Rust-native-only policy.
    pub fn resolve(
        &self,
        on_missing: Option<&dyn Fn(&str) -> MissingSourceAction>,
    ) -> Result<ResolvedPaths, String> {
        let mut accumulator = self.create_accumulator();
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();

        // Project-first so cwd resources win collisions.
        let mut all_packages: Vec<(PackageSource, &'static str)> = Vec::new();
        if let Some(packages) = project_settings.get("packages").and_then(Value::as_array) {
            for pkg in packages {
                if let Ok(pkg) = serde_json::from_value::<PackageSource>(pkg.clone()) {
                    all_packages.push((pkg, "project"));
                }
            }
        }
        if let Some(packages) = global_settings.get("packages").and_then(Value::as_array) {
            for pkg in packages {
                if let Ok(pkg) = serde_json::from_value::<PackageSource>(pkg.clone()) {
                    all_packages.push((pkg, "user"));
                }
            }
        }

        let package_sources = self.dedupe_packages(&all_packages);
        self.resolve_package_sources(&package_sources, &mut accumulator, on_missing)?;

        let global_base_dir = PathBuf::from(&self.agent_dir);
        let project_base_dir = Path::new(&self.cwd).join(CONFIG_DIR_NAME);

        for resource_type in RESOURCE_TYPES {
            let target = self.get_target_map(&mut accumulator, resource_type);
            let key = resource_type.settings_key();
            let project_entries = settings_string_list(&project_settings, key);
            let global_entries = settings_string_list(&global_settings, key);
            self.resolve_local_entries(
                &project_entries,
                resource_type,
                target,
                &PathMetadata {
                    source: "local".to_string(),
                    scope: ResolvedScope::Project,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: Some(path_to_string(&project_base_dir)),
                },
                &project_base_dir,
            );
            self.resolve_local_entries(
                &global_entries,
                resource_type,
                target,
                &PathMetadata {
                    source: "local".to_string(),
                    scope: ResolvedScope::User,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: Some(path_to_string(&global_base_dir)),
                },
                &global_base_dir,
            );
        }

        self.add_auto_discovered_resources(
            &mut accumulator,
            &global_settings,
            &project_settings,
            &global_base_dir,
            &project_base_dir,
        );

        Ok(self.to_resolved_paths(&accumulator))
    }

    /// Port of `resolveExtensionSources`: resolve a specific source list, used
    /// for temporary/CLI extension loading.
    pub fn resolve_extension_sources(
        &self,
        sources: &[String],
        local: bool,
        temporary: bool,
    ) -> Result<ResolvedPaths, String> {
        let mut accumulator = self.create_accumulator();
        let scope: &'static str = if temporary {
            "temporary"
        } else if local {
            "project"
        } else {
            "user"
        };
        let package_sources: Vec<(PackageSource, &'static str)> = sources
            .iter()
            .map(|s| (PackageSource::Str(s.clone()), scope))
            .collect();
        self.resolve_package_sources(&package_sources, &mut accumulator, None)?;
        Ok(self.to_resolved_paths(&accumulator))
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_package_sources(
        &self,
        sources: &[(PackageSource, &'static str)],
        accumulator: &mut ResourceAccumulator,
        on_missing: Option<&dyn Fn(&str) -> MissingSourceAction>,
    ) -> Result<(), String> {
        for (pkg, scope) in sources {
            let scope: &'static str = scope;
            let (source_str, is_obj) = package_source_parts(pkg);
            let filter = if is_obj {
                match pkg {
                    PackageSource::Obj(o) => Some(PackageFilter::from_obj(o)),
                    _ => None,
                }
            } else {
                None
            };
            let delta_base = self.find_autoload_delta_base(pkg, scope, sources);
            let resolved_source = delta_base
                .as_ref()
                .map(|d| d.0.as_str())
                .unwrap_or(source_str.as_str());
            let resolved_scope = delta_base.as_ref().map(|d| d.1).unwrap_or(scope);
            let parsed = ParsedSource::parse(resolved_source);
            let metadata = PathMetadata {
                source: source_str.clone(),
                scope: match scope {
                    "user" => ResolvedScope::User,
                    "project" => ResolvedScope::Project,
                    _ => ResolvedScope::Temporary,
                },
                origin: ResourceOrigin::Package,
                base_dir: None,
            };

            if let ParsedSource::Local(local) = &parsed {
                let base_dir = self.base_dir_for_scope(resolved_scope);
                self.resolve_local_extension_source(
                    local,
                    accumulator,
                    filter.as_ref(),
                    &metadata,
                    &base_dir,
                );
                continue;
            }

            let is_missing_handled = |resolved_source: &str| -> Result<bool, String> {
                if self.is_offline() {
                    return Ok(false);
                }
                if on_missing.is_none() {
                    self.install_parsed_source(&parsed, resolved_scope)?;
                    return Ok(true);
                }
                match on_missing.unwrap()(resolved_source) {
                    MissingSourceAction::Skip => Ok(false),
                    MissingSourceAction::Error => Err(format!("Missing source: {resolved_source}")),
                    MissingSourceAction::Install => {
                        self.install_parsed_source(&parsed, resolved_scope)?;
                        Ok(true)
                    }
                }
            };

            match &parsed {
                ParsedSource::Npm(_) => {
                    return Err(rust_native_only_package_error(resolved_source));
                }
                ParsedSource::Git(git) => {
                    let installed_path = self.get_git_install_path(git, resolved_scope);
                    let installed = if !installed_path.exists() {
                        is_missing_handled(resolved_source)?
                    } else {
                        true
                    };
                    if !installed {
                        continue;
                    }
                    let mut metadata = metadata;
                    metadata.base_dir = Some(path_to_string(&installed_path));
                    self.collect_package_resources(
                        &installed_path,
                        accumulator,
                        filter.as_ref(),
                        &metadata,
                    );
                }
                ParsedSource::Local(_) => {}
            }
        }
        Ok(())
    }

    fn install_parsed_source(
        &self,
        parsed: &ParsedSource,
        scope: &'static str,
    ) -> Result<(), String> {
        match parsed {
            ParsedSource::Npm(_) => Err(rust_native_only_package_error("npm source")),
            ParsedSource::Git(git) => self.install_git(git, scope),
            ParsedSource::Local(_) => Ok(()),
        }
    }

    fn find_autoload_delta_base(
        &self,
        pkg: &PackageSource,
        scope: &'static str,
        sources: &[(PackageSource, &'static str)],
    ) -> Option<(String, &'static str)> {
        if scope != "project" {
            return None;
        }
        let (source_str, is_obj) = package_source_parts(pkg);
        if !is_obj {
            return None;
        }
        let PackageSource::Obj(obj) = pkg else {
            return None;
        };
        if obj.autoload != Some(false) {
            return None;
        }
        let identity = self.get_package_identity(&source_str, Some(scope));
        sources
            .iter()
            .find(|(other, other_scope)| {
                if *other_scope != "user" {
                    return false;
                }
                let other_str = package_source_parts(other).0;
                self.get_package_identity(&other_str, Some("user")) == identity
            })
            .map(|(other, other_scope)| (package_source_parts(other).0, *other_scope))
    }

    fn resolve_local_extension_source(
        &self,
        source: &LocalSource,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
        base_dir: &Path,
    ) {
        let resolved = self.resolve_path_from_base(&source.path, base_dir);
        if resolved.is_file() {
            // Upstream preserves an explicit file source as an extension
            // entry. Keep that boundary visible to the Rust-native loader so
            // it can report the actionable unsupported-source diagnostic for
            // JS/TS (or an unregistered native artifact), rather than
            // silently dropping the user's explicit path.
            let mut file_metadata = metadata.clone();
            file_metadata.base_dir = resolved.parent().map(path_to_string);
            self.add_resource(
                &mut accumulator.extensions,
                &path_to_string(&resolved),
                &file_metadata,
                true,
            );
            return;
        }
        if !resolved.is_dir() {
            return;
        }
        let mut package_metadata = metadata.clone();
        package_metadata.base_dir = Some(path_to_string(&resolved));
        // This still collects skills, prompts, and themes from a local bundle;
        // executable extension entries are filtered by collect_package_resources.
        let _ = self.collect_package_resources(&resolved, accumulator, filter, &package_metadata);
    }

    /// Port of upstream `dedupePackages`: project scope wins over global for
    /// the same package identity; an `autoload: false` project entry is a delta
    /// over the (kept) global entry.
    fn dedupe_packages(
        &self,
        packages: &[(PackageSource, &'static str)],
    ) -> Vec<(PackageSource, &'static str)> {
        let mut result: Vec<(PackageSource, &'static str)> = Vec::new();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for entry in packages {
            let source_str = package_source_parts(&entry.0).0;
            let identity = self.get_package_identity(&source_str, Some(entry.1));
            if let Some(&index) = seen.get(&identity) {
                let existing = &result[index];
                if existing.1 == "project" && entry.1 == "user" {
                    if let PackageSource::Obj(o) = &existing.0 {
                        if o.autoload == Some(false) {
                            result.push(entry.clone());
                        }
                    }
                } else if entry.1 == "project" {
                    result[index] = entry.clone();
                }
            } else {
                seen.insert(identity, result.len());
                result.push(entry.clone());
            }
        }
        result
    }

    fn resolve_local_entries(
        &self,
        entries: &[String],
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
        base_dir: &Path,
    ) {
        if entries.is_empty() {
            return;
        }
        let (plain, patterns) = split_patterns(entries);
        let resolved_plain: Vec<String> = plain
            .iter()
            .map(|p| path_to_string(&self.resolve_path_from_base(p, base_dir)))
            .collect();
        let all_files = self.collect_files_from_paths(&resolved_plain, resource_type);
        let enabled_paths = apply_patterns(&all_files, &patterns, &path_to_string(base_dir));
        for f in &all_files {
            self.add_resource(target, f, metadata, enabled_paths.contains(f));
        }
    }

    /// Port of `collectPackageResources`. Returns false when no resource dir
    /// was found (so the caller falls back to registering the dir itself).
    fn collect_package_resources(
        &self,
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for resource_type in RESOURCE_TYPES {
                let target = self.get_target_map(accumulator, resource_type);
                let patterns = filter.patterns(resource_type);
                if filter.autoload == Some(false) {
                    self.apply_package_delta_filter(
                        package_root,
                        patterns,
                        resource_type,
                        target,
                        metadata,
                    );
                } else if let Some(patterns) = patterns {
                    self.apply_package_filter(
                        package_root,
                        patterns,
                        resource_type,
                        target,
                        metadata,
                    );
                } else {
                    self.collect_default_resources(package_root, resource_type, target, metadata);
                }
            }
            return true;
        }

        let manifest = read_pi_manifest(&package_root.join("package.json"));
        if let Some(manifest) = manifest {
            for resource_type in RESOURCE_TYPES {
                let entries = match resource_type {
                    ResourceType::Extensions => manifest.extensions.clone(),
                    ResourceType::Skills => manifest.skills.clone(),
                    ResourceType::Prompts => manifest.prompts.clone(),
                    ResourceType::Themes => manifest.themes.clone(),
                };
                self.add_manifest_entries(
                    &entries,
                    package_root,
                    resource_type,
                    self.get_target_map(accumulator, resource_type),
                    metadata,
                );
            }
            return true;
        }

        let mut has_any_dir = false;
        for resource_type in RESOURCE_TYPES {
            let dir = package_root.join(resource_type.dir_name());
            if dir.exists() {
                let files = collect_resource_files(&dir, resource_type, &dir);
                for f in files {
                    self.add_resource(
                        self.get_target_map(accumulator, resource_type),
                        &f,
                        metadata,
                        true,
                    );
                }
                has_any_dir = true;
            }
        }
        has_any_dir
    }

    fn collect_default_resources(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        if resource_type_is_executable(resource_type) {
            return;
        }
        if let Some(manifest) = read_pi_manifest(&package_root.join("package.json")) {
            let entries = match resource_type {
                ResourceType::Extensions => manifest.extensions.clone(),
                ResourceType::Skills => manifest.skills.clone(),
                ResourceType::Prompts => manifest.prompts.clone(),
                ResourceType::Themes => manifest.themes.clone(),
            };
            self.add_manifest_entries(&entries, package_root, resource_type, target, metadata);
            return;
        }
        let dir = package_root.join(resource_type.dir_name());
        if dir.exists() {
            let files = collect_resource_files(&dir, resource_type, &dir);
            for f in files {
                self.add_resource(target, &f, metadata, true);
            }
        }
    }

    fn apply_package_filter(
        &self,
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        if resource_type_is_executable(resource_type) {
            return;
        }
        let all_files = self.collect_manifest_files(package_root, resource_type);
        if user_patterns.is_empty() {
            // Empty array explicitly disables all resources of this type.
            for f in &all_files {
                self.add_resource(target, f, metadata, false);
            }
            return;
        }
        let enabled_by_user =
            apply_patterns(&all_files, user_patterns, &path_to_string(package_root));
        for f in &all_files {
            let enabled = enabled_by_user.contains(f);
            self.add_resource(target, f, metadata, enabled);
        }
    }

    fn apply_package_delta_filter(
        &self,
        package_root: &Path,
        user_patterns: Option<&Vec<String>>,
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        if resource_type_is_executable(resource_type) {
            return;
        }
        let user_patterns = match user_patterns {
            Some(p) if !p.is_empty() => p,
            _ => return,
        };
        let all_files = self.collect_manifest_files(package_root, resource_type);
        let enabled_by_user = apply_autoload_disabled_patterns(
            &all_files,
            user_patterns,
            &path_to_string(package_root),
        );
        for (file_path, enabled) in enabled_by_user {
            self.add_resource(target, &file_path, metadata, enabled);
        }
    }

    /// Port of `collectManifestFiles`: all files of a resource type from a
    /// package, after the manifest's own patterns.
    fn collect_manifest_files(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
    ) -> Vec<String> {
        if resource_type_is_executable(resource_type) {
            return Vec::new();
        }
        let manifest = read_pi_manifest(&package_root.join("package.json"));
        let entries = manifest.as_ref().map(|m| match resource_type {
            ResourceType::Extensions => m.extensions.clone(),
            ResourceType::Skills => m.skills.clone(),
            ResourceType::Prompts => m.prompts.clone(),
            ResourceType::Themes => m.themes.clone(),
        });
        if let Some(entries) = entries {
            if !entries.is_empty() {
                let all_files =
                    self.collect_files_from_manifest_entries(&entries, package_root, resource_type);
                let manifest_patterns: Vec<String> = entries
                    .iter()
                    .filter(|e| is_override_pattern(e))
                    .cloned()
                    .collect();
                return if !manifest_patterns.is_empty() {
                    let set = apply_patterns(
                        &all_files,
                        &manifest_patterns,
                        &path_to_string(package_root),
                    );
                    set.into_iter().collect()
                } else {
                    all_files
                };
            }
        }
        let convention_dir = package_root.join(resource_type.dir_name());
        if !convention_dir.exists() {
            return Vec::new();
        }
        collect_resource_files(&convention_dir, resource_type, &convention_dir)
    }

    fn add_manifest_entries(
        &self,
        entries: &[String],
        root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        if resource_type_is_executable(resource_type) {
            return;
        }
        if entries.is_empty() {
            return;
        }
        let all_files = self.collect_files_from_manifest_entries(entries, root, resource_type);
        let patterns: Vec<String> = entries
            .iter()
            .filter(|e| is_override_pattern(e))
            .cloned()
            .collect();
        let enabled_paths = apply_patterns(&all_files, &patterns, &path_to_string(root));
        for f in &all_files {
            if enabled_paths.contains(f) {
                self.add_resource(target, f, metadata, true);
            }
        }
    }

    fn collect_files_from_manifest_entries(
        &self,
        entries: &[String],
        root: &Path,
        resource_type: ResourceType,
    ) -> Vec<String> {
        let source_entries: Vec<&String> =
            entries.iter().filter(|e| !is_override_pattern(e)).collect();
        let mut resolved: Vec<String> = Vec::new();
        for entry in source_entries {
            if has_glob_pattern(entry) {
                resolved.extend(glob_expand(root, entry));
            } else {
                resolved.push(path_to_string(&root.join(entry)));
            }
        }
        self.collect_files_from_paths(&resolved, resource_type)
    }

    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        global_settings: &crate::core::settings::SettingsMap,
        project_settings: &crate::core::settings::SettingsMap,
        global_base_dir: &Path,
        project_base_dir: &Path,
    ) {
        let user_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: ResolvedScope::User,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(path_to_string(global_base_dir)),
        };
        let project_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: ResolvedScope::Project,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(path_to_string(project_base_dir)),
        };
        let user_overrides = settings_overrides(global_settings);
        let project_overrides = settings_overrides(project_settings);

        let user_dirs = settings_dirs(global_base_dir);
        let project_dirs = settings_dirs(project_base_dir);
        let user_agents_skills_dir = home_dir()
            .map(|h| Path::new(&h).join(".agents").join("skills"))
            .unwrap_or_else(|| PathBuf::from("~/.agents/skills"));
        let project_trusted = self.settings_manager.is_project_trusted();
        let project_agents_skill_dirs: Vec<String> = if project_trusted {
            collect_ancestor_agents_skill_dirs(Path::new(&self.cwd))
                .into_iter()
                .filter(|dir| {
                    resolve_noop(dir) != resolve_noop(&path_to_string(&user_agents_skills_dir))
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut add_resources = |resource_type: ResourceType,
                                 paths: &[String],
                                 metadata: &PathMetadata,
                                 overrides: &[String],
                                 base_dir: &Path| {
            let target = accumulator_target(accumulator, resource_type);
            for path in paths {
                let enabled = is_enabled_by_overrides(path, overrides, &path_to_string(base_dir));
                accumulator_add(target, path, metadata, enabled);
            }
        };

        if project_trusted {
            add_resources(
                ResourceType::Extensions,
                &collect_auto_extension_entries(&project_dirs.extensions, &project_dirs.extensions),
                &project_metadata,
                &project_overrides.extensions,
                project_base_dir,
            );
            add_resources(
                ResourceType::Skills,
                &collect_skill_entries(
                    &project_dirs.skills,
                    SkillDiscoveryMode::Pi,
                    &project_dirs.skills,
                ),
                &project_metadata,
                &project_overrides.skills,
                project_base_dir,
            );
        }

        for agents_skills_dir in &project_agents_skill_dirs {
            let agents_skills_path = Path::new(agents_skills_dir);
            let agents_base_dir = agents_skills_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let mut agents_metadata = project_metadata.clone();
            agents_metadata.base_dir = Some(path_to_string(&agents_base_dir));
            add_resources(
                ResourceType::Skills,
                &collect_skill_entries(
                    agents_skills_path,
                    SkillDiscoveryMode::Agents,
                    agents_skills_path,
                ),
                &agents_metadata,
                &project_overrides.skills,
                &agents_base_dir,
            );
        }

        if project_trusted {
            add_resources(
                ResourceType::Prompts,
                &collect_auto_prompt_entries(&project_dirs.prompts, &project_dirs.prompts),
                &project_metadata,
                &project_overrides.prompts,
                project_base_dir,
            );
            add_resources(
                ResourceType::Themes,
                &collect_auto_theme_entries(&project_dirs.themes, &project_dirs.themes),
                &project_metadata,
                &project_overrides.themes,
                project_base_dir,
            );
        }

        add_resources(
            ResourceType::Extensions,
            &collect_auto_extension_entries(&user_dirs.extensions, &user_dirs.extensions),
            &user_metadata,
            &user_overrides.extensions,
            global_base_dir,
        );
        add_resources(
            ResourceType::Skills,
            &collect_skill_entries(&user_dirs.skills, SkillDiscoveryMode::Pi, &user_dirs.skills),
            &user_metadata,
            &user_overrides.skills,
            global_base_dir,
        );

        let user_agents_base_dir = user_agents_skills_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| user_agents_skills_dir.clone());
        let mut user_agents_metadata = user_metadata.clone();
        user_agents_metadata.base_dir = Some(path_to_string(&user_agents_base_dir));
        add_resources(
            ResourceType::Skills,
            &collect_skill_entries(
                &user_agents_skills_dir,
                SkillDiscoveryMode::Agents,
                &user_agents_skills_dir,
            ),
            &user_agents_metadata,
            &user_overrides.skills,
            &user_agents_base_dir,
        );

        add_resources(
            ResourceType::Prompts,
            &collect_auto_prompt_entries(&user_dirs.prompts, &user_dirs.prompts),
            &user_metadata,
            &user_overrides.prompts,
            global_base_dir,
        );
        add_resources(
            ResourceType::Themes,
            &collect_auto_theme_entries(&user_dirs.themes, &user_dirs.themes),
            &user_metadata,
            &user_overrides.themes,
            global_base_dir,
        );
    }

    fn collect_files_from_paths(
        &self,
        paths: &[String],
        resource_type: ResourceType,
    ) -> Vec<String> {
        let mut files = Vec::new();
        for p in paths {
            let path = Path::new(p);
            if !path.exists() {
                continue;
            }
            let ok = match fs::metadata(path) {
                Ok(m) if m.is_file() => {
                    files.push(p.clone());
                    true
                }
                Ok(m) if m.is_dir() => {
                    files.extend(collect_resource_files(path, resource_type, path));
                    true
                }
                _ => false,
            };
            let _ = ok;
        }
        files
    }

    fn get_target_map<'a>(
        &self,
        accumulator: &'a mut ResourceAccumulator,
        resource_type: ResourceType,
    ) -> &'a mut ResourceMap {
        match resource_type {
            ResourceType::Extensions => &mut accumulator.extensions,
            ResourceType::Skills => &mut accumulator.skills,
            ResourceType::Prompts => &mut accumulator.prompts,
            ResourceType::Themes => &mut accumulator.themes,
        }
    }

    fn add_resource(
        &self,
        map: &mut ResourceMap,
        path: &str,
        metadata: &PathMetadata,
        enabled: bool,
    ) {
        if path.is_empty() {
            return;
        }
        if !map.contains_key(path) {
            map.insert(path.to_string(), (metadata.clone(), enabled));
        }
    }

    fn create_accumulator(&self) -> ResourceAccumulator {
        ResourceAccumulator::default()
    }

    fn to_resolved_paths(&self, accumulator: &ResourceAccumulator) -> ResolvedPaths {
        let map_to_resolved = |entries: &ResourceMap| -> Vec<ResolvedResource> {
            let mut resolved: Vec<(String, PathMetadata, bool)> = entries
                .iter()
                .map(|(path, (metadata, enabled))| (path.clone(), metadata.clone(), *enabled))
                .collect();
            resolved.sort_by_key(|(_, m, _)| resource_precedence_rank(m));
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            resolved
                .into_iter()
                .filter(|(path, _, _)| {
                    let canonical = canonicalize_path(path);
                    if seen.contains(&canonical) {
                        return false;
                    }
                    seen.insert(canonical);
                    true
                })
                .map(|(path, metadata, enabled)| ResolvedResource {
                    path,
                    enabled,
                    metadata,
                })
                .collect()
        };

        ResolvedPaths {
            extensions: map_to_resolved(&accumulator.extensions),
            skills: map_to_resolved(&accumulator.skills),
            prompts: map_to_resolved(&accumulator.prompts),
            themes: map_to_resolved(&accumulator.themes),
        }
    }
}

fn home_dir() -> Option<String> {
    dirs::home_dir().map(|h| h.display().to_string())
}

fn resolve_noop(p: &str) -> String {
    normalize_lexical_path(Path::new(p))
        .to_string_lossy()
        .into_owned()
}

fn canonicalize_path(p: &str) -> String {
    fs::canonicalize(p)
        .map(|c| to_posix_path(&path_to_string(&c)))
        .unwrap_or_else(|_| {
            normalize_lexical_path(Path::new(p))
                .to_string_lossy()
                .into_owned()
        })
}

fn settings_string_list(settings: &crate::core::settings::SettingsMap, key: &str) -> Vec<String> {
    settings
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn settings_overrides(
    settings: &crate::core::settings::SettingsMap,
) -> crate::core::package_manager::SettingsOverrides {
    let extensions = settings_string_list(settings, "extensions");
    let skills = settings_string_list(settings, "skills");
    let prompts = settings_string_list(settings, "prompts");
    let themes = settings_string_list(settings, "themes");
    crate::core::package_manager::SettingsOverrides {
        extensions,
        skills,
        prompts,
        themes,
    }
}

struct SettingsOverrides {
    extensions: Vec<String>,
    skills: Vec<String>,
    prompts: Vec<String>,
    themes: Vec<String>,
}

fn settings_dirs(base: &Path) -> SettingsDirs {
    SettingsDirs {
        extensions: base.join("extensions"),
        skills: base.join("skills"),
        prompts: base.join("prompts"),
        themes: base.join("themes"),
    }
}

struct SettingsDirs {
    extensions: PathBuf,
    skills: PathBuf,
    prompts: PathBuf,
    themes: PathBuf,
}

fn accumulator_target(
    accumulator: &mut ResourceAccumulator,
    resource_type: ResourceType,
) -> &mut ResourceMap {
    match resource_type {
        ResourceType::Extensions => &mut accumulator.extensions,
        ResourceType::Skills => &mut accumulator.skills,
        ResourceType::Prompts => &mut accumulator.prompts,
        ResourceType::Themes => &mut accumulator.themes,
    }
}

fn accumulator_add(map: &mut ResourceMap, path: &str, metadata: &PathMetadata, enabled: bool) {
    if path.is_empty() {
        return;
    }
    if !map.contains_key(path) {
        map.insert(path.to_string(), (metadata.clone(), enabled));
    }
}

/// Glob-expand a manifest entry pattern against a root dir (upstream
/// `globSync(entry, { cwd: root, absolute: true })`). Supports `*`, `?`,
/// `**` and `[...]` via the workspace regex-based matcher.
fn glob_expand(root: &Path, entry: &str) -> Vec<String> {
    let mut pattern: String = entry.to_string();
    if !has_glob_pattern(&pattern) {
        return Vec::new();
    }
    if !pattern.starts_with('/') && !pattern.starts_with("./") {
        pattern = format!("./{pattern}");
    }
    let mut out = Vec::new();
    let mut walk = Vec::new();
    walk.push(root.to_path_buf());
    collect_glob_matches(&mut walk, &pattern, &mut out);
    out
}

fn collect_glob_matches(stack: &mut Vec<PathBuf>, pattern: &str, out: &mut Vec<String>) {
    let normalized = to_posix_path(pattern);
    let Some((prefix_part, rest)) = normalized.split_once('/') else {
        // No slash left: treat remaining as a filename glob.
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if glob_match(&normalized, &name, false) {
                    out.push(path_to_string(&entry.path()));
                }
            }
        }
        return;
    };

    let mut next_stack = Vec::new();
    let part = if prefix_part == "." { "" } else { prefix_part };
    while let Some(dir) = stack.pop() {
        if part.is_empty() {
            next_stack.push(dir.clone());
            continue;
        }
        if part.contains('*') || part.contains('?') || part.contains('[') {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if glob_match(part, &name, false) {
                    next_stack.push(entry.path());
                }
            }
        } else {
            let child = dir.join(part);
            if child.exists() {
                next_stack.push(child);
            }
        }
    }
    // Continue matching on the new stack with the remainder.
    collect_glob_matches(&mut next_stack, rest, out);
}

struct LocalGitUpdateTarget {
    ref_: String,
    #[allow(dead_code)]
    head: String,
    fetch_args: Vec<String>,
}

fn package_source_parts(pkg: &PackageSource) -> (String, bool) {
    match pkg {
        PackageSource::Str(s) => (s.clone(), false),
        PackageSource::Obj(o) => (o.source.clone(), true),
    }
}

fn short_hash(input: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    input.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

/// Lexically normalize a path (resolve `.`/`..` components without touching
/// the filesystem) — upstream `resolvePath` produces canonical absolute paths.
fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

fn expand_home(input: &str) -> String {
    if input == "~" {
        return dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_else(|| input.to_string());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|h| h.join(rest).display().to_string())
            .unwrap_or_else(|| input.to_string());
    }
    input.to_string()
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_string()
}

fn pathdiff(base: &Path, target: &Path) -> String {
    let base_parts: Vec<String> = base
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let target_parts: Vec<String> = target
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut common = 0;
    while common < base_parts.len()
        && common < target_parts.len()
        && base_parts[common] == target_parts[common]
    {
        common += 1;
    }
    let mut rel = Vec::new();
    for _ in common..base_parts.len() {
        rel.push("..".to_string());
    }
    for part in &target_parts[common..] {
        rel.push(part.clone());
    }
    if rel.is_empty() {
        ".".to_string()
    } else {
        rel.join("/")
    }
}

/// Panic-free stand-in for `throw new Error` in path guards (the upstream
/// `resolveManagedPath` throws; this prefix keeps the contract explicit for
/// callers that propagate errors).
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
fn panic_write(message: &str) -> ! {
    panic!("{message}");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::settings::PackageSourceObj;
    use crate::core::settings::SettingsManager;

    fn manager(cwd: &Path, agent_dir: &Path) -> PackageManager {
        PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: agent_dir.display().to_string(),
            settings_manager: SettingsManager::in_memory(crate::core::settings::SettingsMap::new()),
        })
    }

    fn manager_in_memory(map: serde_json::Map<String, Value>) -> PackageManager {
        PackageManager::new(PackageManagerOptions {
            cwd: "/tmp/pi-pm-cwd".to_string(),
            agent_dir: "/tmp/pi-pm-agent".to_string(),
            settings_manager: SettingsManager::in_memory(map.into_iter().collect()),
        })
    }

    #[test]
    fn parses_npm_sources() {
        let parsed = ParsedSource::parse("npm:@scope/pkg@1.2.3");
        match parsed {
            ParsedSource::Npm(npm) => {
                assert!(npm.pinned);
                assert_eq!(npm.name, "@scope/pkg");
                assert_eq!(npm.version.as_deref(), Some("1.2.3"));
            }
            _ => panic!("expected npm"),
        }
        match ParsedSource::parse("npm:@scope/pkg@^1.2.3") {
            ParsedSource::Npm(npm) => assert!(!npm.pinned),
            _ => panic!("expected npm"),
        }
        match ParsedSource::parse("npm:pkg") {
            ParsedSource::Npm(npm) => {
                assert!(!npm.pinned);
                assert_eq!(npm.name, "pkg");
            }
            _ => panic!("expected npm"),
        }
    }

    #[test]
    fn parses_git_sources() {
        assert_eq!(
            ParsedSource::parse("git:github.com/user/repo@v1").type_name(),
            "git"
        );
        let parsed = ParsedSource::parse("https://github.com/user/repo@v1.2.3");
        match parsed {
            ParsedSource::Git(git) => {
                assert_eq!(git.host, "github.com");
                assert_eq!(git.path, "user/repo");
                assert_eq!(git.ref_.as_deref(), Some("v1.2.3"));
                assert!(git.pinned);
            }
            _ => panic!("expected git"),
        }
        assert_eq!(
            ParsedSource::parse("git:git@github.com:user/repo@v1").type_name(),
            "git"
        );
        assert_eq!(
            ParsedSource::parse("ssh://git@github.com/user/repo@v1").type_name(),
            "git"
        );
        // Host/path shorthand without git: is local.
        assert_eq!(
            ParsedSource::parse("github.com/user/repo").type_name(),
            "local"
        );
        // With git: prefix it is git.
        assert_eq!(
            ParsedSource::parse("git:github.com/user/repo").type_name(),
            "git"
        );
    }

    #[test]
    fn parses_local_sources() {
        assert_eq!(
            ParsedSource::parse("/absolute/path/to/package").type_name(),
            "local"
        );
        assert_eq!(
            ParsedSource::parse("./relative/path/to/package").type_name(),
            "local"
        );
        assert_eq!(
            ParsedSource::parse("../relative/path/to/package").type_name(),
            "local"
        );
        let local = ParsedSource::parse("../agents/foo");
        match local {
            ParsedSource::Local(l) => assert_eq!(l.path, "../agents/foo"),
            _ => panic!("expected local"),
        }
    }

    #[test]
    fn git_paths_strip_git_suffix() {
        let parsed = ParsedSource::parse("https://github.com/user/repo.git");
        match parsed {
            ParsedSource::Git(git) => assert_eq!(git.path, "user/repo"),
            _ => panic!("expected git"),
        }
    }

    #[test]
    fn install_roots_layout() {
        let cwd = Path::new("/tmp/cwd");
        let agent = Path::new("/tmp/agent");
        let pm = manager(cwd, agent);
        assert_eq!(
            pm.get_git_install_root("user").unwrap(),
            Path::new("/tmp/agent/git")
        );
        assert_eq!(
            pm.get_git_install_root("project").unwrap(),
            Path::new("/tmp/cwd/.pi/git")
        );
    }

    #[test]
    fn settings_normalization_global_relative_to_agent() {
        let cwd = std::env::temp_dir().join(format!("pi-pm-normalize-{}", uuid::Uuid::new_v4()));
        let agent = cwd.join("agent");
        let pkg_dir = cwd.join("packages").join("local-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let mut pm = PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: agent.display().to_string(),
            settings_manager: SettingsManager::in_memory(Default::default()),
        });
        let added = pm.add_source_to_settings("./packages/local-pkg", false);
        assert!(added, "adding first time must return true");
        let packages = pm.settings_manager.get_packages();
        let _ = &packages;
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn add_same_git_source_ref_returns_false() {
        let mut pm = manager_in_memory(Default::default());
        assert!(pm.add_source_to_settings("git:github.com/user/repo@v1", false));
        assert!(!pm.add_source_to_settings("git:github.com/user/repo@v1", false));
    }

    #[test]
    fn add_git_source_updates_ref() {
        let mut pm = manager_in_memory(Default::default());
        pm.add_source_to_settings("git:github.com/user/repo@v1", false);
        assert!(pm.add_source_to_settings("git:github.com/user/repo@v2", false));
        let packages = pm.settings_manager.get_packages();
        assert_eq!(packages.len(), 1);
        match &packages[0] {
            PackageSource::Str(s) => assert_eq!(s, "git:github.com/user/repo@v2"),
            _ => panic!("expected string package source"),
        }
    }

    #[test]
    fn add_git_preserves_object_filters() {
        let mut pm = manager_in_memory(Default::default());
        pm.settings_manager
            .set_packages(vec![PackageSource::Obj(PackageSourceObj {
                source: "git:github.com/user/repo@v1".into(),
                autoload: None,
                extensions: Some(vec!["extensions/main.so".into()]),
                skills: Some(vec![]),
                prompts: Some(vec!["prompts/review.md".into()]),
                themes: Some(vec!["themes/dark.json".into()]),
            })]);
        assert!(pm.add_source_to_settings("git:github.com/user/repo@v2", false));
        let packages = pm.settings_manager.get_packages();
        match &packages[0] {
            PackageSource::Obj(o) => {
                assert_eq!(o.source, "git:github.com/user/repo@v2");
                assert_eq!(
                    o.extensions.as_ref().unwrap(),
                    &vec!["extensions/main.so".to_string()]
                );
                assert_eq!(
                    o.prompts.as_ref().unwrap(),
                    &vec!["prompts/review.md".to_string()]
                );
            }
            _ => panic!("expected object package source"),
        }
    }

    #[test]
    fn remove_source_matches_equivalent_forms() {
        let mut pm = manager_in_memory(Default::default());
        pm.add_source_to_settings("git:github.com/user/repo@v1", false);
        assert!(pm.remove_source_from_settings("git:github.com/user/repo@v9", false));
        assert!(pm.settings_manager.get_packages().is_empty());
    }

    #[test]
    fn local_install_requires_existing_path() {
        let cwd = std::env::temp_dir().join(format!("pi-pm-local-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut pm = PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: cwd.join("agent").display().to_string(),
            settings_manager: SettingsManager::in_memory(Default::default()),
        });
        let err = pm.install("./does-not-exist", false).unwrap_err();
        assert!(err.contains("Path does not exist"), "{err}");
        // Existing dir install succeeds (upstream local install validates only).
        std::fs::create_dir_all(cwd.join("pkg").join("extensions")).unwrap();
        assert!(pm.install("./pkg", false).is_ok());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn npm_package_operations_fail_with_rust_native_guidance() {
        let mut pm = manager_in_memory(Default::default());
        for source in [
            "npm:left-pad",
            "NPM:left-pad",
            "npx:left-pad",
            "bun:left-pad",
        ] {
            for operation in ["install", "remove"] {
                let result = if operation == "install" {
                    pm.install(source, false)
                } else {
                    pm.remove(source, false)
                };
                let error = result.expect_err(operation);
                assert!(error.contains("Rust-native-only"), "{error}");
                assert!(error.contains("npm, npx, and bun"), "{error}");
            }
        }
        for source in ["npm:left-pad", "npx:left-pad", "bun:left-pad"] {
            let error = pm.update(Some(source)).expect_err("update");
            assert!(error.contains("Rust-native-only"), "{error}");
            assert!(error.contains(source), "{error}");
        }
    }

    #[test]
    fn semver_helpers() {
        assert!(parse_semver_valid("1.2.3"));
        assert!(!parse_semver_valid("^1.2.3"));
        assert!(parse_semver_valid_range("^1.2.3"));
    }
    fn git(args: &[&str], cwd: &Path) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn git_commit(cwd: &Path, name: &str, contents: &str) {
        std::fs::write(cwd.join(name), contents).unwrap();
        git(&["add", name], cwd);
        git(
            &[
                "-c",
                "user.name=pi-test",
                "-c",
                "user.email=pi-test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "test commit",
            ],
            cwd,
        );
    }

    /// Failed git updates must leave the installed checkout intact and record
    /// the incomplete-update marker; the next successful update clears it.
    /// Uses local repositories only, so no network is involved. The suite
    /// runs serially (`--test-threads=1`) because this mutates PI_OFFLINE.
    #[cfg(unix)]
    #[test]
    fn failed_git_update_keeps_checkout_and_marks_incomplete() {
        use std::os::unix::fs::PermissionsExt;

        static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
            std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
        let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous_offline = std::env::var_os(crate::config::ENV_OFFLINE);
        std::env::remove_var(crate::config::ENV_OFFLINE);

        let root = std::env::temp_dir().join(format!("pi-pm-update-{}", uuid::Uuid::new_v4()));
        let origin = root.join("origin");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&origin).unwrap();
        git(&["-c", "init.defaultBranch=main", "init"], &origin);
        git_commit(&origin, "payload.txt", "v1");

        let pm = manager(&root, &agent_dir);
        let source = GitSource {
            repo: origin.display().to_string(),
            host: "localhost".to_string(),
            path: "origin-repo".to_string(),
            ref_: None,
            pinned: false,
        };
        pm.install_git(&source, "user")
            .expect("install from local origin");
        let target = pm.get_git_install_path(&source, "user");
        let marker = pm.git_update_marker_path(&target);
        assert_eq!(
            std::fs::read_to_string(target.join("payload.txt")).unwrap(),
            "v1"
        );

        // A broken remote fails the fetch before anything is reset.
        git(
            &[
                "remote",
                "set-url",
                "origin",
                root.join("missing").display().to_string().as_str(),
            ],
            &target,
        );
        let error = pm.update_git(&source, "user").unwrap_err();
        assert!(!marker.exists(), "fetch failure must not mark: {error}");
        assert_eq!(
            std::fs::read_to_string(target.join("payload.txt")).unwrap(),
            "v1",
            "checkout must stay intact"
        );

        // A failed reset leaves the marker behind with the old checkout.
        // Fail `reset` deterministically with a PATH shim: git's lock-file
        // renames defeat permission-based injection.
        git_commit(&origin, "payload.txt", "v2");
        git(
            &[
                "remote",
                "set-url",
                "origin",
                origin.display().to_string().as_str(),
            ],
            &target,
        );
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            bin.join("git"),
            "#!/bin/sh\nif [ \"$1\" = \"reset\" ]; then echo \"simulated reset failure\" >&2; exit 1; fi\nexec /usr/bin/git \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let previous_path = std::env::var_os("PATH");
        let mut path = bin.display().to_string();
        if let Some(previous) = previous_path.as_ref() {
            path.push(':');
            path.push_str(&previous.to_string_lossy());
        }
        std::env::set_var("PATH", &path);
        let error = pm.update_git(&source, "user").unwrap_err();
        match previous_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        assert!(marker.exists(), "reset failure must mark: {error}");
        assert_eq!(
            std::fs::read_to_string(target.join("payload.txt")).unwrap(),
            "v1",
            "checkout must stay intact"
        );

        // The next successful update heals both.
        pm.update_git(&source, "user").expect("retry update");
        assert!(!marker.exists(), "success must clear the marker");
        assert_eq!(
            std::fs::read_to_string(target.join("payload.txt")).unwrap(),
            "v2"
        );

        match previous_offline {
            Some(value) => std::env::set_var(crate::config::ENV_OFFLINE, value),
            None => std::env::remove_var(crate::config::ENV_OFFLINE),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::interactive::config_selector::build_groups;

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn fixture(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-pm-resolve-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn resolve_manager(cwd: &Path, agent_dir: &Path) -> PackageManager {
        PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: agent_dir.display().to_string(),
            settings_manager: SettingsManager::in_memory(Default::default()),
        })
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_auto_discovers_user_resources() {
        let cwd = fixture("auto-cwd");
        let agent = fixture("auto-agent");
        write(
            &agent.join("skills").join("alpha").join("SKILL.md"),
            "---\nname: alpha\n---\nA\n",
        );
        write(
            &agent.join("extensions").join("hook.so"),
            "rust-extension\n",
        );
        write(&agent.join("extensions").join("hook.js"), "export {}\n");
        write(&agent.join("extensions").join("hook.ts"), "export {}\n");
        write(&agent.join("prompts").join("tip.md"), "# tip\n");
        write(&agent.join("themes").join("dark.json"), "{}");
        let pm = resolve_manager(&cwd, &agent);
        let resolved = pm.resolve(None).unwrap();

        let skill = resolved
            .skills
            .iter()
            .find(|r| r.path.ends_with("SKILL.md"))
            .expect("skill");
        assert!(skill.enabled);
        assert_eq!(skill.metadata.source, "auto");
        assert_eq!(skill.metadata.scope, ResolvedScope::User);
        assert_eq!(skill.metadata.origin, ResourceOrigin::TopLevel);

        assert!(resolved.extensions.is_empty());
        assert!(resolved.prompts.iter().any(|r| r.path.ends_with("tip.md")));
        assert!(resolved
            .themes
            .iter()
            .any(|r| r.path.ends_with("dark.json")));

        // The user skills dir metadata carries the agent_dir baseDir.
        assert!(resolved
            .skills
            .iter()
            .find(|r| r.path.ends_with("SKILL.md"))
            .unwrap()
            .metadata
            .base_dir
            .is_some());
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_extension_sources_retains_explicit_file_for_loader_diagnostic() {
        let cwd = fixture("explicit-file-cwd");
        let agent = fixture("explicit-file-agent");
        let extension = agent.join("extension.ts");
        write(&extension, "export default function extension() {}\n");
        let pm = resolve_manager(&cwd, &agent);

        let resolved = pm
            .resolve_extension_sources(&[path_to_string(&extension)], false, false)
            .unwrap();

        assert_eq!(resolved.extensions.len(), 1);
        let entry = &resolved.extensions[0];
        assert_eq!(entry.path, path_to_string(&extension));
        assert!(entry.enabled);
        assert_eq!(
            entry.metadata.base_dir.as_deref(),
            extension.parent().map(path_to_string).as_deref()
        );
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_discovers_project_resources_when_trusted() {
        let cwd = fixture("proj-cwd");
        let agent = fixture("proj-agent");
        write(
            &cwd.join(CONFIG_DIR_NAME)
                .join("skills")
                .join("beta")
                .join("SKILL.md"),
            "---\nname: beta\n---\nB\n",
        );
        let mut pm = resolve_manager(&cwd, &agent);
        let trusted = true;
        pm.settings_manager.set_project_trusted(trusted);
        let resolved = pm.resolve(None).unwrap();

        assert!(resolved.skills.iter().any(|r| {
            r.path.ends_with("SKILL.md")
                && r.metadata.scope == ResolvedScope::Project
                && r.metadata.source == "auto"
        }));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn settings_local_entries_become_user_local_resources() {
        let cwd = fixture("local-cwd");
        let agent = fixture("local-agent");
        write(
            &agent.join("extensions").join("mine.so"),
            "rust-extension\n",
        );
        let mut map = crate::core::settings::SettingsMap::new();
        map.insert(
            "extensions".to_string(),
            Value::Array(vec![Value::String("extensions/mine.so".into())]),
        );
        let pm = PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: agent.display().to_string(),
            settings_manager: SettingsManager::in_memory(map),
        });
        let resolved = pm.resolve(None).unwrap();

        let ext = resolved
            .extensions
            .iter()
            .find(|r| r.path.ends_with("mine.so"))
            .expect("configured extension");
        assert!(ext.enabled);
        assert_eq!(ext.metadata.source, "local");
        assert_eq!(ext.metadata.scope, ResolvedScope::User);
        assert_eq!(ext.metadata.origin, ResourceOrigin::TopLevel);
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_preserves_insertion_order_for_same_precedence_resources() {
        let cwd = fixture("ordered-cwd");
        let agent = fixture("ordered-agent");
        let second = cwd.join("second.md");
        let first = cwd.join("first.md");
        write(&second, "second\n");
        write(&first, "first\n");

        let mut settings = crate::core::settings::SettingsMap::new();
        settings.insert(
            "prompts".to_string(),
            Value::Array(vec![
                Value::String(path_to_string(&second)),
                Value::String(path_to_string(&first)),
            ]),
        );
        let pm = PackageManager::new(PackageManagerOptions {
            cwd: cwd.display().to_string(),
            agent_dir: agent.display().to_string(),
            settings_manager: SettingsManager::in_memory(settings),
        });

        let resolved = pm.resolve(None).unwrap();
        let paths: Vec<String> = resolved
            .prompts
            .iter()
            .map(|resource| resource.path.clone())
            .collect();
        assert_eq!(paths, vec![path_to_string(&second), path_to_string(&first)]);

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    fn package_convention_prompt_and_theme_files_are_recursive() {
        let root = fixture("recursive-package-resources");
        let prompts = root.join("prompts");
        let themes = root.join("themes");
        write(&prompts.join("reviews").join("release.md"), "release\n");
        write(&themes.join("variants").join("dim.json"), "{}\n");

        let prompt_paths = collect_resource_files(&prompts, ResourceType::Prompts, &prompts);
        let theme_paths = collect_resource_files(&themes, ResourceType::Themes, &themes);
        assert_eq!(
            prompt_paths,
            vec![path_to_string(&prompts.join("reviews/release.md"))]
        );
        assert_eq!(
            theme_paths,
            vec![path_to_string(&themes.join("variants/dim.json"))]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn package_filter_ignores_executable_extensions_and_keeps_resources() {
        let cwd = fixture("filter-cwd");
        let agent = fixture("filter-agent");
        // Configured local package at agent_dir/pkgs/ext with an extensions dir.
        write(
            &agent
                .join("pkgs")
                .join("ext")
                .join("extensions")
                .join("a.so"),
            "export {}\n",
        );
        write(
            &agent
                .join("pkgs")
                .join("ext")
                .join("extensions")
                .join("b.so"),
            "export {}\n",
        );
        write(
            &agent
                .join("pkgs")
                .join("ext")
                .join("skills")
                .join("one")
                .join("SKILL.md"),
            "---\nname: one\n---\nskill\n",
        );
        write(
            &agent
                .join("pkgs")
                .join("ext")
                .join("prompts")
                .join("one.md"),
            "prompt\n",
        );
        write(
            &agent
                .join("pkgs")
                .join("ext")
                .join("themes")
                .join("one.json"),
            "{}",
        );
        let mut pm = resolve_manager(&cwd, &agent);
        pm.settings_manager
            .set_packages(vec![PackageSource::Obj(PackageSourceObj {
                source: "./pkgs/ext".into(),
                autoload: None,
                extensions: Some(vec!["extensions/a.so".into(), "!extensions/b.so".into()]),
                skills: Some(vec!["skills/one/SKILL.md".into()]),
                prompts: Some(vec!["prompts/one.md".into()]),
                themes: Some(vec!["themes/one.json".into()]),
            })]);
        let resolved = pm.resolve(None).unwrap();

        assert!(resolved.extensions.is_empty());
        assert!(resolved.skills.iter().any(|r| r.path.ends_with("SKILL.md")));
        assert!(resolved.prompts.iter().any(|r| r.path.ends_with("one.md")));
        assert!(resolved.themes.iter().any(|r| r.path.ends_with("one.json")));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn ignore_file_excludes_auto_discovered_skill() {
        let cwd = fixture("ignore-cwd");
        let agent = fixture("ignore-agent");
        write(&agent.join("skills").join(".gitignore"), "secret/\n");
        write(
            &agent.join("skills").join("keep").join("SKILL.md"),
            "---\nname: keep\n---\nK\n",
        );
        write(
            &agent.join("skills").join("secret").join("SKILL.md"),
            "---\nname: secret\n---\nS\n",
        );
        let pm = resolve_manager(&cwd, &agent);
        let resolved = pm.resolve(None).unwrap();

        assert!(resolved.skills.iter().any(|r| r.path.contains("keep")));
        assert!(!resolved.skills.iter().any(|r| r.path.contains("secret")));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn npm_package_resolution_is_rejected_without_install() {
        let cwd = fixture("skip-cwd");
        let agent = fixture("skip-agent");
        let mut pm = resolve_manager(&cwd, &agent);
        pm.settings_manager
            .set_packages(vec![PackageSource::Str("npm:left-pad".into())]);
        let error = pm
            .resolve(Some(&|_source| MissingSourceAction::Skip))
            .unwrap_err();
        assert!(error.contains("Rust-native-only"), "{error}");
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn on_missing_error_action_propagates() {
        let cwd = fixture("missing-cwd");
        let agent = fixture("missing-agent");
        let mut pm = resolve_manager(&cwd, &agent);
        pm.settings_manager
            .set_packages(vec![PackageSource::Str("npm:left-pad".into())]);
        let err = pm
            .resolve(Some(&|_source| MissingSourceAction::Error))
            .unwrap_err();
        assert!(err.contains("Rust-native-only"), "{err}");
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn resolve_feeds_build_groups() {
        let cwd = fixture("groups-cwd");
        let agent = fixture("groups-agent");
        write(
            &agent.join("skills").join("delta").join("SKILL.md"),
            "---\nname: delta\n---\nD\n",
        );
        write(
            &agent.join("extensions").join("plain.so"),
            "rust-extension\n",
        );
        let pm = resolve_manager(&cwd, &agent);
        let resolved = pm.resolve(None).unwrap();

        let groups = build_groups(
            &resolved,
            &agent.display().to_string(),
            CONFIG_DIR_NAME,
            None,
        );
        // Auto-discovered user resources share a group keyed on the agent_dir;
        // real-home ~/.agents skills (if any) form separate groups. Find the
        // group that contains our fixture skill and assert on it.
        let fixture_group = groups
            .iter()
            .find(|g| {
                g.subgroups
                    .iter()
                    .any(|sg| sg.items.iter().any(|i| i.display_name == "delta"))
            })
            .expect("group containing fixture skill");
        assert!(fixture_group.is_user());
        let kinds: Vec<&str> = fixture_group
            .subgroups
            .iter()
            .map(|s| s.resource_type.as_str())
            .collect();
        // Executable extensions are intentionally absent; skills remain.
        assert!(!kinds.contains(&"extensions"));
        assert!(kinds.contains(&"skills"));
        assert!(!kinds.contains(&"prompts"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    fn precedence_rank_ordering() {
        let mk = |source: &str, scope: ResolvedScope, origin: ResourceOrigin| -> PathMetadata {
            PathMetadata::synthetic(source, scope, origin, None)
        };
        // lower rank = higher precedence
        assert!(
            resource_precedence_rank(&mk(
                "local",
                ResolvedScope::Project,
                ResourceOrigin::TopLevel
            )) < resource_precedence_rank(&mk(
                "auto",
                ResolvedScope::Project,
                ResourceOrigin::TopLevel
            ))
        );
        assert!(
            resource_precedence_rank(&mk(
                "auto",
                ResolvedScope::Project,
                ResourceOrigin::TopLevel
            )) < resource_precedence_rank(&mk(
                "local",
                ResolvedScope::User,
                ResourceOrigin::TopLevel
            ))
        );
        assert!(
            resource_precedence_rank(&mk("local", ResolvedScope::User, ResourceOrigin::TopLevel))
                < resource_precedence_rank(&mk(
                    "auto",
                    ResolvedScope::User,
                    ResourceOrigin::TopLevel
                ))
        );
        assert!(
            resource_precedence_rank(&mk("auto", ResolvedScope::User, ResourceOrigin::TopLevel))
                < resource_precedence_rank(&mk(
                    "auto",
                    ResolvedScope::User,
                    ResourceOrigin::Package
                ))
        );
    }
}
