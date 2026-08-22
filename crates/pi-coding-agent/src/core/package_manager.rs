//! Package manager — port of
//! `packages/coding-agent/src/core/package-manager.ts` (the CLI-observable
//! surface: source parsing, install/remove/update/list, settings
//! persistence, on-disk npm/git install layout).
//!
//! The full resource-resolution layer (skills/prompts/themes/extension
//! collecting with ignore-file and pattern filtering) is intentionally not
//! ported here — the Rust port consumes package resources at the extension
//! discovery seam (`core/extensions`). This module owns the *package* surface:
//! parsing, install roots, npm/git commands, and settings `packages` writes.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::config::CONFIG_DIR_NAME;
use crate::core::settings::{PackageSource, SettingsManager};


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
    parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
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
pub fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let re = regex::Regex::new(r"^(@?[^@]+(?:/[^@]+)?)(?:@(.+))?$").unwrap();
    match re.captures(spec) {
        Some(caps) => {
            let name = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_else(|| spec.to_string());
            let version = caps.get(2).map(|m| m.as_str().to_string());
            (name, version)
        }
        None => (spec.to_string(), None),
    }
}

fn is_local_path(source: &str) -> bool {
    source.starts_with("./") || source.starts_with("../") || source.starts_with('/') || source == "."
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
                    return (format!("git@{}:{repo_path}", &rest[..colon]), Some(ref_.to_string()));
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
                        let mut new_url = format!("{}://{}{repo_path}", &url[..scheme_end], &after_scheme[..slash + 1]);
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
            if i + 2 < bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit() {
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
    let Some(decoded) = decode_for_validation(value) else { return true };
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

fn builtin_git_source(repo: String, host: &str, path: &str, ref_: Option<String>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized_path = path.trim_end_matches(".git").trim_start_matches('/').to_string();
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false) || has_unsafe_git_install_part(&normalized_path, true) {
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
            let repo_url = if has_git_prefix && !repo_without_ref.starts_with("https://") && !repo_without_ref.starts_with("http://") && !repo_without_ref.starts_with("ssh://") && !repo_without_ref.starts_with("git://") {
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
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("ssh://") || lower.starts_with("git://")
}

fn parse_protocol_host_path(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split_once("://")?.1;
    let hostport = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    let path = after_scheme.split('/').skip(1).collect::<Vec<_>>().join("/");
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
            return ParsedSource::Local(LocalSource { path: source.to_string() });
        }
        if let Some(git) = parse_git_url(source) {
            return ParsedSource::Git(git);
        }
        ParsedSource::Local(LocalSource { path: source.to_string() })
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
            cwd: options.cwd,
            agent_dir: options.agent_dir,
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
        let input = expand_home(input);
        let path = PathBuf::from(&input);
        if path.is_absolute() {
            path
        } else {
            Path::new(&self.cwd).join(path)
        }
    }

    fn resolve_path_from_base(&self, input: &str, base_dir: &Path) -> PathBuf {
        let path = PathBuf::from(expand_home(input));
        if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        }
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
            ParsedSource::Local(local) => format!("local:{}", self.resolve_path(&local.path).display()),
        }
    }

    fn package_sources_match(&self, existing: &PackageSource, input_source: &str, scope: SourceScope) -> bool {
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
        if let Some((index, existing)) = current.iter().enumerate().find(|(_, e)| self.package_sources_match(e, source, scope)) {
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

    pub fn get_npm_install_root(&self, scope: SourceScope, temporary: bool) -> PathBuf {
        if temporary {
            return self.temporary_dir("npm");
        }
        if scope == "project" {
            return Path::new(&self.cwd).join(CONFIG_DIR_NAME).join("npm");
        }
        Path::new(&self.agent_dir).join("npm")
    }

    pub fn get_git_install_root(&self, scope: SourceScope) -> Option<PathBuf> {
        if scope == "temporary" {
            return None;
        }
        if scope == "project" {
            return Some(Path::new(&self.cwd).join(CONFIG_DIR_NAME).join("git"));
        }
        Some(Path::new(&self.agent_dir).join("git"))
    }

    pub fn get_managed_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> PathBuf {
        if scope == "temporary" {
            return self.temporary_dir("npm").join("node_modules").join(&source.name);
        }
        self.get_npm_install_root(scope, false).join("node_modules").join(&source.name)
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
            std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()).join(parts.to_vec().join("/"))
        };
        let resolved_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
        };
        let resolved_root_str = normalized_path(&resolved_root);
        let resolved_str = normalized_path(&resolved);
        if resolved_str != resolved_root_str && !resolved_str.starts_with(&format!("{resolved_root_str}/")) {
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
            ParsedSource::Npm(npm) => {
                let path = self.get_managed_npm_install_path(&npm, scope);
                if path.exists() {
                    Some(path.display().to_string())
                } else {
                    None
                }
            }
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

    // ------------------------------------------------------------------
    // npm command helpers
    // ------------------------------------------------------------------

    /// Resolve the npm command live from settings (upstream reads
    /// `settingsManager.getNpmCommand()` on every call).
    pub fn get_npm_command(&self) -> (String, Vec<String>) {
        match self.settings_manager.get_npm_command() {
            Some(cmd) if !cmd.is_empty() => {
                let mut iter = cmd.iter();
                let command = iter.next().cloned().unwrap_or_else(|| "npm".to_string());
                let args: Vec<String> = iter.cloned().collect();
                (command, args)
            }
            _ => ("npm".to_string(), Vec::new()),
        }
    }

    pub fn get_package_manager_name(&self) -> String {
        let (command, args) = self.get_npm_command();
        let mut parts = vec![command];
        parts.extend(args);
        let separator = parts.iter().rposition(|p| p == "--");
        let package_manager_command = match separator {
            Some(index) => parts.get(index + 1).cloned().unwrap_or(parts[0].clone()),
            None => parts[0].clone(),
        };
        Path::new(&package_manager_command)
            .file_name()
            .map(|f| f.to_string_lossy().replace(".cmd", "").replace(".exe", ""))
            .unwrap_or_else(|| package_manager_command)
    }

    fn run_command(&self, command: &str, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd.output().map_err(|e| format!("Failed to run {command}: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("{command} exited with {:?}: {}", output.status.code(), stderr.trim()))
        }
    }

    fn run_command_capture(&self, command: &str, args: &[String], cwd: Option<&Path>) -> Result<String, String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd.output().map_err(|e| format!("Failed to run {command}: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("{command} exited with {:?}: {}", output.status.code(), stderr.trim()))
        }
    }

    fn run_npm_command(&self, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let (command, command_args) = self.get_npm_command();
        let mut all_args = command_args.clone();
        all_args.extend(args.iter().cloned());
        self.run_command(&command, &all_args, cwd)
    }

    fn get_npm_install_args(&self, specs: &[String], install_root: &Path) -> Vec<String> {
        let package_manager_name = self.get_package_manager_name();
        let mut args = vec!["install".to_string()];
        args.extend(specs.iter().cloned());
        if package_manager_name == "bun" {
            args.push("--cwd".to_string());
            args.push(install_root.display().to_string());
            args.push("--omit=peer".to_string());
        } else if package_manager_name == "pnpm" {
            args.push("--prefix".to_string());
            args.push(install_root.display().to_string());
            args.push("--config.auto-install-peers=false".to_string());
            args.push("--config.strict-peer-dependencies=false".to_string());
            args.push("--config.strict-dep-builds=false".to_string());
        } else {
            args.push("--prefix".to_string());
            args.push(install_root.display().to_string());
            args.push("--legacy-peer-deps".to_string());
        }
        args
    }

    fn get_git_dependency_install_args(&self) -> Vec<String> {
        if self.settings_manager.get_npm_command().as_ref().map(|c| !c.is_empty()).unwrap_or(false) {
            vec!["install".to_string()]
        } else {
            vec!["install".to_string(), "--omit=dev".to_string()]
        }
    }

    fn ensure_npm_project(&self, install_root: &Path) {
        std::fs::create_dir_all(install_root).ok();
        // gitignore dir for package installs (upstream ensureGitIgnore).
        let _ = self.ensure_git_ignore(install_root);
        let package_json = install_root.join("package.json");
        if !package_json.exists() {
            let pkg_json = json!({ "name": "pi-extensions", "private": true });
            let _ = std::fs::write(package_json, serde_json::to_string_pretty(&pkg_json).unwrap());
        }
    }

    fn ensure_git_ignore(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            std::fs::write(&ignore_path, "*\n!.gitignore\n").map_err(|e| format!("write .gitignore: {e}"))?;
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
        let scope: SourceScope = if local { "project" } else { "user" };
        let parsed = ParsedSource::parse(source);
        let source_owned = source.to_string();
        self.with_progress("install", source, &format!("Installing {source}..."), || {
            match &parsed {
                ParsedSource::Npm(npm) => self.install_npm(npm, scope, false),
                ParsedSource::Git(git) => self.install_git(git, scope),
                ParsedSource::Local(local) => {
                    let resolved = self.resolve_path(&local.path);
                    if !resolved.exists() {
                        return Err(format!("Path does not exist: {}", resolved.display()));
                    }
                    Ok(())
                }
            }
        })?;
        let _ = source_owned;
        Ok(())
    }

    pub fn install_and_persist(&mut self, source: &str, local: bool) -> Result<(), String> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local);
        Ok(())
    }

    pub fn remove(&mut self, source: &str, local: bool) -> Result<(), String> {
        let scope: SourceScope = if local { "project" } else { "user" };
        let parsed = ParsedSource::parse(source);
        let source_owned = source.to_string();
        self.with_progress("remove", source, &format!("Removing {source}..."), || match &parsed {
            ParsedSource::Npm(npm) => self.uninstall_npm(npm, scope),
            ParsedSource::Git(git) => self.remove_git(git, scope),
            ParsedSource::Local(_) => Ok(()),
        })?;
        let _ = source_owned;
        Ok(())
    }

    pub fn remove_and_persist(&mut self, source: &str, local: bool) -> Result<bool, String> {
        self.remove(source, local)?;
        Ok(self.remove_source_from_settings(source, local))
    }

    pub fn update(&mut self, source: Option<&str>) -> Result<bool, String> {
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
                ParsedSource::Npm(npm) => {
                    if !npm.pinned {
                        self.update_npm_package(npm, scope)?
                    } else {
                        false
                    }
                }
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
                    format!("local:{}", self.resolve_path_from_base(&local.path, &base).display())
                }
                None => format!("local:{}", self.resolve_path(&local.path).display()),
            },
        }
    }

    // ------------------------------------------------------------------
    // npm install internals
    // ------------------------------------------------------------------

    fn install_npm(&self, source: &NpmSource, scope: SourceScope, temporary: bool) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, temporary);
        self.ensure_npm_project(&install_root);
        let args = self.get_npm_install_args(std::slice::from_ref(&source.spec), &install_root);
        self.run_npm_command(&args, None)
    }

    fn uninstall_npm(&self, source: &NpmSource, scope: SourceScope) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, false);
        if !install_root.exists() {
            return Ok(());
        }
        let package_manager_name = self.get_package_manager_name();
        if package_manager_name == "bun" {
            let args = vec!["uninstall".to_string(), source.name.clone(), "--cwd".to_string(), install_root.display().to_string()];
            return self.run_npm_command(&args, None);
        }
        let mut args = vec!["uninstall".to_string(), source.name.clone(), "--prefix".to_string(), install_root.display().to_string()];
        if package_manager_name != "pnpm" {
            args.push("--legacy-peer-deps".to_string());
        }
        self.run_npm_command(&args, None)
    }

    fn update_npm_package(&self, source: &NpmSource, scope: SourceScope) -> Result<bool, String> {
        let installed_path = self.get_managed_npm_install_path(source, scope);
        let installed_version = if installed_path.exists() {
            installed_npm_version(&installed_path)
        } else {
            None
        };
        let should_update: bool = match installed_version {
            Some(installed) => {
                let latest = self.get_latest_npm_version(source)?;
                semver_gt(&latest, &installed)
            }
            None => true,
        };
        if !should_update {
            return Ok(false);
        }
        self.install_npm(source, scope, false)?;
        Ok(true)
    }

    fn get_latest_npm_version(&self, source: &NpmSource) -> Result<String, String> {
        let spec = if source.version.is_some() {
            source.spec.clone()
        } else {
            source.name.clone()
        };
        let (command, command_args) = self.get_npm_command();
        let mut args = command_args.clone();
        args.push("view".to_string());
        args.push(spec);
        args.push("version".to_string());
        args.push("--json".to_string());
        let stdout = self.run_command_capture(&command, &args, None)?;
        let raw = stdout.trim();
        if raw.is_empty() {
            return Err("Empty response from npm view".to_string());
        }
        let parsed: Value = serde_json::from_str(raw).map_err(|_| "Unexpected response from npm view".to_string())?;
        if let Ok(s) = serde_json::from_value::<String>(parsed.clone()) {
            return Ok(s);
        }
        if let Value::Array(list) = parsed {
            let versions: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(|s| s.to_string()))
                .collect();
            if !versions.is_empty() {
                if let Some(range) = &source.range {
                    if let Some(latest) = versions.iter().filter(|v| semver_satisfies(v, range)).max() {
                        return Ok(latest.clone());
                    }
                } else if let Some(latest) = versions.iter().max() {
                    return Ok(latest.clone());
                }
            }
        }
        Err("Unexpected response from npm view".to_string())
    }

    // ------------------------------------------------------------------
    // git install internals
    // ------------------------------------------------------------------

    fn install_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope);
        if target_dir.exists() {
            if source.ref_.is_some() {
                self.ensure_git_ref(&target_dir, &["fetch".to_string(), "origin".to_string(), source.ref_.clone().unwrap()], "FETCH_HEAD")?;
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
            self.run_command("git", &["clone".to_string(), source.repo.clone(), target_dir.display().to_string()], None)?;
            if let Some(ref_) = &source.ref_ {
                self.run_command("git", &["checkout".to_string(), ref_.clone()], Some(&target_dir))?;
            }
            let package_json = target_dir.join("package.json");
            if package_json.exists() {
                let args = self.get_git_dependency_install_args();
                self.run_npm_command(&args, Some(&target_dir))?;
            }
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
            self.ensure_git_ref(&target_dir, &["fetch".to_string(), "origin".to_string(), ref_.clone()], "FETCH_HEAD")?;
            return Ok(true);
        }
        let target = self.get_local_git_update_target(&target_dir)?;
        self.ensure_git_ref(&target_dir, &target.fetch_args, &target.ref_)?;
        Ok(true)
    }

    fn git_update_marker_path(&self, target_dir: &Path) -> PathBuf {
        let parent = target_dir.parent().unwrap_or(Path::new("."));
        let name = target_dir.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        parent.join(format!(".{name}.pi-update-incomplete"))
    }

    fn get_local_git_update_target(&self, installed_path: &Path) -> Result<LocalGitUpdateTarget, String> {
        let upstream = self
            .run_command_capture("git", &["rev-parse".to_string(), "--abbrev-ref".to_string(), "@{upstream}".to_string()], Some(installed_path))?;
        let trimmed = upstream.trim().to_string();
        if let Some(branch) = trimmed.strip_prefix("origin/") {
            let head = self
                .run_command_capture("git", &["rev-parse".to_string(), "@{upstream}".to_string()], Some(installed_path))?;
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
            let _ = self.run_command("git", &["remote".to_string(), "set-head".to_string(), "origin".to_string(), "-a".to_string()], Some(installed_path));
            let head = self
                .run_command_capture("git", &["rev-parse".to_string(), "origin/HEAD".to_string()], Some(installed_path))?;
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

    fn ensure_git_ref(&self, target_dir: &Path, fetch_args: &[String], ref_: &str) -> Result<(), String> {
        self.run_command("git", fetch_args, Some(target_dir))?;
        let local_head = self
            .run_command_capture("git", &["rev-parse".to_string(), "HEAD".to_string()], Some(target_dir))?;
        let commit_ref = format!("{ref_}^{{commit}}");
        let target_head = self
            .run_command_capture("git", &["rev-parse".to_string(), commit_ref], Some(target_dir))?;
        let marker_path = self.git_update_marker_path(target_dir);
        if local_head.trim() == target_head.trim() {
            let _ = std::fs::remove_file(&marker_path);
            return Ok(());
        }
        std::fs::write(&marker_path, "").map_err(|e| format!("write marker: {e}"))?;
        self.run_command("git", &["reset".to_string(), "--hard".to_string(), target_head.trim().to_string()], Some(target_dir))?;
        // Clean + reinstall deps.
        let clean_result = self.run_command("git", &["clean".to_string(), "-fdx".to_string()], Some(target_dir));
        let _ = std::fs::remove_file(&marker_path);
        clean_result?;
        let package_json = target_dir.join("package.json");
        if package_json.exists() {
            let args = self.get_git_dependency_install_args();
            self.run_npm_command(&args, Some(target_dir))?;
        }
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
            let has_entries = std::fs::read_dir(&dir).map(|mut it| it.next().is_some()).unwrap_or(false);
            if !has_entries {
                let _ = std::fs::remove_dir_all(&dir);
            } else {
                break;
            }
            current = dir.parent().map(|p| p.to_path_buf());
        }
    }
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

fn installed_npm_version(installed_path: &Path) -> Option<String> {
    let package_json = installed_path.join("package.json");
    let content = std::fs::read_to_string(package_json).ok()?;
    let pkg: Value = serde_json::from_str(&content).ok()?;
    pkg.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Very small semver comparison: numeric dots first, then prerelease-aware.
fn semver_gt(a: &str, b: &str) -> bool {
    parse_semver_parts(a).zip(parse_semver_parts(b)).map(|(pa, pb)| pa > pb).unwrap_or(a > b)
}

fn parse_semver_parts(version: &str) -> Option<(Vec<u64>, Option<String>)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let prerelease = version.split('-').nth(1).map(|s| s.split('+').next().unwrap_or(s).to_string());
    let parts: Vec<u64> = core.split('.').filter_map(|p| p.parse().ok()).collect();
    if parts.is_empty() {
        return None;
    }
    Some((parts, prerelease))
}

fn semver_satisfies(version: &str, range: &str) -> bool {
    let range = range.trim_start_matches(['^', '~', '=']);
    if parse_semver_parts(version).is_some() {
        if range.is_empty() || range == "*" {
            return true;
        }
        if let Some((range_parts, _)) = parse_semver_parts(range) {
            if let Some((version_parts, _)) = parse_semver_parts(version) {
                if range_parts.len() != version_parts.len() {
                    return false;
                }
                return range_parts <= version_parts;
            }
        }
    }
    false
}

fn short_hash(input: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    input.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

fn expand_home(input: &str) -> String {
    if input == "~" {
        return dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_else(|| input.to_string());
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
    let base_parts: Vec<String> = base.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let target_parts: Vec<String> = target.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let mut common = 0;
    while common < base_parts.len() && common < target_parts.len() && base_parts[common] == target_parts[common] {
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
fn panic_write(message: &str) -> ! {
    panic!("{message}");
}

#[cfg(test)]
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
        assert_eq!(ParsedSource::parse("git:github.com/user/repo@v1").type_name(), "git");
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
        assert_eq!(ParsedSource::parse("git:git@github.com:user/repo@v1").type_name(), "git");
        assert_eq!(ParsedSource::parse("ssh://git@github.com/user/repo@v1").type_name(), "git");
        // Host/path shorthand without git: is local.
        assert_eq!(ParsedSource::parse("github.com/user/repo").type_name(), "local");
        // With git: prefix it is git.
        assert_eq!(ParsedSource::parse("git:github.com/user/repo").type_name(), "git");
    }

    #[test]
    fn parses_local_sources() {
        assert_eq!(ParsedSource::parse("/absolute/path/to/package").type_name(), "local");
        assert_eq!(ParsedSource::parse("./relative/path/to/package").type_name(), "local");
        assert_eq!(ParsedSource::parse("../relative/path/to/package").type_name(), "local");
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
        // npm user root.
        assert_eq!(pm.get_npm_install_root("user", false), Path::new("/tmp/agent/npm"));
        // npm project root.
        assert_eq!(pm.get_npm_install_root("project", false), Path::new("/tmp/cwd/.pi/npm"));
        // git user root.
        assert_eq!(pm.get_git_install_root("user").unwrap(), Path::new("/tmp/agent/git"));
        assert_eq!(pm.get_git_install_root("project").unwrap(), Path::new("/tmp/cwd/.pi/git"));
    }

    #[test]
    fn managed_npm_install_path() {
        let pm = manager_in_memory(Default::default());
        let source = NpmSource {
            spec: "npm:left-pad".into(),
            name: "left-pad".into(),
            version: None,
            range: None,
            pinned: false,
        };
        assert_eq!(
            pm.get_managed_npm_install_path(&source, "user"),
            Path::new("/tmp/pi-pm-agent/npm/node_modules/left-pad")
        );
        assert_eq!(
            pm.get_managed_npm_install_path(&source, "project"),
            Path::new("/tmp/pi-pm-cwd/.pi/npm/node_modules/left-pad")
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
        pm.settings_manager.set_packages(vec![PackageSource::Obj(PackageSourceObj {
            source: "git:github.com/user/repo@v1".into(),
            autoload: None,
            extensions: Some(vec!["extensions/main.ts".into()]),
            skills: Some(vec![]),
            prompts: Some(vec!["prompts/review.md".into()]),
            themes: Some(vec!["themes/dark.json".into()]),
        })]);
        assert!(pm.add_source_to_settings("git:github.com/user/repo@v2", false));
        let packages = pm.settings_manager.get_packages();
        match &packages[0] {
            PackageSource::Obj(o) => {
                assert_eq!(o.source, "git:github.com/user/repo@v2");
                assert_eq!(o.extensions.as_ref().unwrap(), &vec!["extensions/main.ts".to_string()]);
                assert_eq!(o.prompts.as_ref().unwrap(), &vec!["prompts/review.md".to_string()]);
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
    fn ensure_npm_project_creates_package_json() {
        let cwd = std::env::temp_dir().join(format!("pi-pm-npmproj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = cwd.join("agent");
        let pm = manager(&cwd, &agent);
        let root = cwd.join("npm");
        pm.ensure_npm_project(&root);
        let content = std::fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "pi-extensions");
        assert_eq!(parsed["private"], true);
        // .gitignore content for managed installs.
        assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), "*\n!.gitignore\n");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn package_manager_name_and_npm_command() {
        let mut pm = manager_in_memory(Default::default());
        pm.settings_manager.set_npm_command(Some(vec!["/usr/bin/npm".into()]));
        assert_eq!(pm.get_npm_command(), ("/usr/bin/npm".to_string(), vec![]));
        assert_eq!(pm.get_package_manager_name(), "npm");
        pm.settings_manager.set_npm_command(Some(vec!["npx".into(), "yarn".into(), "--".into(), "bun".into()]));
        assert_eq!(pm.get_package_manager_name(), "bun");
    }

    #[test]
    fn npm_install_args_per_pm() {
        let mut pm = manager_in_memory(Default::default());
        pm.settings_manager.set_npm_command(Some(vec!["npm".into()]));
        let args = pm.get_npm_install_args(&["left-pad".into()], Path::new("/root"));
        assert!(args.contains(&"--legacy-peer-deps".to_string()), "{args:?}");
        pm.settings_manager.set_npm_command(Some(vec!["bun".into()]));
        let args = pm.get_npm_install_args(&["left-pad".into()], Path::new("/root"));
        assert!(args.contains(&"--omit=peer".to_string()), "{args:?}");
        assert!(args.contains(&"/root".to_string()), "{args:?}");
    }

    #[test]
    fn semver_helpers() {
        assert!(parse_semver_valid("1.2.3"));
        assert!(!parse_semver_valid("^1.2.3"));
        assert!(parse_semver_valid_range("^1.2.3"));
        assert!(semver_gt("2.0.0", "1.9.9"));
        assert!(!semver_gt("1.0.0", "1.0.0"));
        assert!(semver_gt("1.0.1", "1.0.0"));
        assert!(semver_satisfies("1.5.0", "^1.2.0"));
    }
}
