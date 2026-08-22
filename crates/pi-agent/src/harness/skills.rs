//! Skills loader — port of `packages/agent/src/harness/skills.ts`.
//!
//! Recursively loads `SKILL.md` (declared) skills and direct root `.md`
//! (inline) skills with skill frontmatter, honoring `.gitignore`/`.ignore`/
//! `.fdignore` per directory, and returns diagnostics for invalid metadata.

use std::path::{Path, PathBuf};

use crate::types::Skill;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// Stable diagnostic codes emitted while loading skills
/// (upstream `SkillDiagnosticCode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub path: String,
}

/// Load skills from one or more directories, recursively. Missing input
/// directories are skipped; invalid declared skill files are diagnostics.
pub fn load_skills(cwd: &str, dirs: &[String]) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for dir in dirs {
        let root = resolve(cwd, dir);
        let meta = match std::fs::metadata(&root) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                diagnostics.push(SkillDiagnostic {
                    code: "file_info_failed",
                    message: e.to_string(),
                    path: dir.clone(),
                });
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }
        let mut matcher = IgnoreMatcher::default();
        let mut out = load_skills_from_dir_internal(&root, true, &mut matcher, &root);
        skills.append(&mut out.skills);
        diagnostics.append(&mut out.diagnostics);
    }
    let _ = cwd;
    (skills, diagnostics)
}

/// Source-tagged variant of [`load_skills`].
pub fn load_sourced_skills<TSource: Clone>(
    cwd: &str,
    inputs: &[(String, TSource)],
) -> (Vec<(Skill, TSource)>, Vec<(SkillDiagnostic, TSource)>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for (path, source) in inputs {
        let (mut sk, mut di) = load_skills(cwd, std::slice::from_ref(path));
        for s in sk.drain(..) {
            skills.push((s, source.clone()));
        }
        for d in di.drain(..) {
            diagnostics.push((d, source.clone()));
        }
    }
    (skills, diagnostics)
}

fn load_skills_from_dir_internal(
    dir: &Path,
    include_root_files: bool,
    matcher: &mut IgnoreMatcher,
    root_dir: &Path,
) -> LoadOutcome {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    let meta = match std::fs::metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome { skills, diagnostics },
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: "file_info_failed",
                message: e.to_string(),
                path: dir.to_string_lossy().into_owned(),
            });
            return LoadOutcome { skills, diagnostics };
        }
    };
    if !meta.is_dir() {
        return LoadOutcome { skills, diagnostics };
    }

    add_ignore_rules(dir, root_dir, matcher, &mut diagnostics);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: "list_failed",
                message: e.to_string(),
                path: dir.to_string_lossy().into_owned(),
            });
            return LoadOutcome { skills, diagnostics };
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }

    // A declared skill directory: the FIRST SKILL.md (sorted or not) short-circuits.
    if names.iter().any(|n| n == "SKILL.md") {
        let full_path = dir.join("SKILL.md");
        if !matcher.ignores(&relative_env_path(root_dir, &full_path)) {
            let result = load_skill_from_file(&full_path, dir);
            if let Some(skill) = result.skill {
                skills.push(skill);
            }
            diagnostics.extend(result.diagnostics);
        }
        return LoadOutcome { skills, diagnostics };
    }

    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    for name in names {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let full_path = dir.join(&name);
        let is_dir = std::fs::metadata(&full_path).map(|m| m.is_dir()).unwrap_or(false);
        let rel = relative_env_path(root_dir, &full_path);
        let ignore_path = if is_dir { format!("{rel}/") } else { rel };
        if matcher.ignores(&ignore_path) {
            continue;
        }
        if is_dir {
            let mut out = load_skills_from_dir_internal(&full_path, false, matcher, root_dir);
            skills.append(&mut out.skills);
            diagnostics.append(&mut out.diagnostics);
            continue;
        }
        if !include_root_files || !name.to_lowercase().ends_with(".md") {
            continue;
        }
        let result = load_skill_from_file(&full_path, dir);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }
    LoadOutcome { skills, diagnostics }
}

struct LoadOutcome {
    skills: Vec<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

fn add_ignore_rules(dir: &Path, root_dir: &Path, matcher: &mut IgnoreMatcher, diagnostics: &mut Vec<SkillDiagnostic>) {
    let relative_dir = relative_env_path(root_dir, dir);
    let prefix = if relative_dir.is_empty() {
        String::new()
    } else {
        format!("{relative_dir}/")
    };
    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        match std::fs::read_to_string(&ignore_path) {
            Ok(content) => {
                for line in content.split('\n') {
                    let line = line.trim_end_matches('\r');
                    if let Some(pattern) = prefix_ignore_pattern(line, &prefix) {
                        matcher.add(&pattern);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                diagnostics.push(SkillDiagnostic {
                    code: "read_failed",
                    message: e.to_string(),
                    path: ignore_path.to_string_lossy().into_owned(),
                });
            }
        }
    }
}

/// Prefix an ignore rule with the current relative directory, preserving
/// negation and escapes (upstream `prefixIgnorePattern`).
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

fn load_skill_from_file(file_path: &Path, parent_dir: &Path) -> SkillFileOutcome {
    let mut diagnostics = Vec::new();
    let is_declared_skill = file_path
        .file_name()
        .map(|n| n == "SKILL.md")
        .unwrap_or(false);
    let raw = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            diagnostics.push(SkillDiagnostic {
                code: "read_failed",
                message: e.to_string(),
                path: file_path.to_string_lossy().into_owned(),
            });
            return SkillFileOutcome { skill: None, diagnostics };
        }
    };
    let Some((frontmatter, body)) = super::frontmatter::parse_frontmatter(&raw) else {
        if is_declared_skill {
            diagnostics.push(SkillDiagnostic {
                code: "parse_failed",
                message: "could not parse YAML frontmatter".to_string(),
                path: file_path.to_string_lossy().into_owned(),
            });
        }
        return SkillFileOutcome { skill: None, diagnostics };
    };

    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if !is_declared_skill && description.as_deref().map(|d| d.trim().is_empty()).unwrap_or(true) {
        return SkillFileOutcome { skill: None, diagnostics };
    }
    for error in validate_description(description.as_deref()) {
        diagnostics.push(SkillDiagnostic {
            code: "invalid_metadata",
            message: error,
            path: file_path.to_string_lossy().into_owned(),
        });
    }

    let parent_dir_name = parent_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let frontmatter_name = frontmatter.get("name").and_then(|v| v.as_str());
    let name = frontmatter_name.map(|s| s.to_string()).unwrap_or_else(|| parent_dir_name.clone());
    for error in validate_name(&name, &parent_dir_name) {
        diagnostics.push(SkillDiagnostic {
            code: "invalid_metadata",
            message: error,
            path: file_path.to_string_lossy().into_owned(),
        });
    }

    let Some(description) = description else {
        return SkillFileOutcome { skill: None, diagnostics };
    };
    if description.trim().is_empty() {
        return SkillFileOutcome { skill: None, diagnostics };
    }

    SkillFileOutcome {
        skill: Some(Skill {
            name,
            description,
            content: body,
            file_path: file_path.to_string_lossy().into_owned(),
            disable_model_invocation: frontmatter.get("disable-model-invocation").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
        diagnostics,
    }
}

struct SkillFileOutcome {
    skill: Option<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!("name \"{name}\" does not match parent directory \"{parent_dir_name}\""));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.chars().count()));
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        errors.push("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None => errors.push("description is required".to_string()),
        Some(d) if d.trim().is_empty() => errors.push("description is required".to_string()),
        Some(d) if d.chars().count() > MAX_DESCRIPTION_LENGTH => {
            errors.push(format!("description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})", d.chars().count()));
        }
        _ => {}
    }
    errors
}

/// Format a skill invocation prompt, optionally appending additional user
/// instructions (upstream `formatSkillInvocation`).
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path,
        dirname_env_path(&skill.file_path),
        skill.content
    );
    match additional_instructions {
        Some(extra) if !extra.is_empty() => format!("{skill_block}\n\n{extra}"),
        _ => skill_block,
    }
}

fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    let separator_index = normalized
        .rfind('/')
        .or_else(|| normalized.rfind('\\'));
    match separator_index {
        Some(1) if normalized.as_bytes().get(1) == Some(&b':') => normalized[..3].to_string(),
        Some(i) if i > 0 => normalized[..i].to_string(),
        _ => "/".to_string(),
    }
}

fn relative_env_path(root: &Path, path: &Path) -> String {
    let norm_root = root.to_string_lossy().replace('\\', "/");
    let norm_root = norm_root.trim_end_matches('/').to_string();
    let norm_path = path.to_string_lossy().replace('\\', "/");
    let norm_path = norm_path.trim_end_matches('/').to_string();
    if norm_path == norm_root {
        return String::new();
    }
    if let Some(rest) = norm_path.strip_prefix(&format!("{norm_root}/")) {
        rest.to_string()
    } else {
        norm_path.trim_start_matches('/').to_string()
    }
}

fn resolve(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Minimal gitignore-style matcher covering the semantics the upstream `ignore`
/// npm package is used for here (line patterns with `#` comments, `!`
/// negations, `\#`/`\!` escapes, `*`/`?`/`**` globs, directory-only trailing
/// `/`, anchored vs relative forms, first-match-wins last-line semantics).
#[derive(Debug, Default)]
pub struct IgnoreMatcher {
    rules: Vec<IgnoreRule>,
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    negated: bool,
    dir_only: bool,
    anchored: bool,
    regex: regex::Regex,
}

impl IgnoreMatcher {
    pub fn add(&mut self, pattern: &str) {
        let Some(rule) = IgnoreRule::compile(pattern) else {
            return;
        };
        self.rules.push(rule);
    }

    pub fn ignores(&self, path: &str) -> bool {
        let p = path.trim_end_matches('/');
        let mut ignored = false;
        for rule in &self.rules {
            let is_dir = path.ends_with('/');
            if rule.dir_only && !is_dir {
                continue;
            }
            let matched = if rule.anchored {
                rule.regex.is_match(p)
            } else {
                rule.regex.is_match(p) || matched_below(&rule.regex, p)
            };
            if matched {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

fn matched_below(re: &regex::Regex, path: &str) -> bool {
    // Unanchored patterns match at any directory level below the rule's prefix
    // — approximated by matching against suffixes (the prefix is already baked
    // into the pattern by prefixIgnorePattern, so match against the full path
    // and, for non-prefixed rules, any suffix).
    if re.is_match(path) {
        return true;
    }
    // Also match if any non-empty suffix matches the regex.
    let bytes = path.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'/' && re.is_match(&path[i + 1..]) {
            return true;
        }
    }
    false
}

impl IgnoreRule {
    fn compile(pattern: &str) -> Option<IgnoreRule> {
        let mut negated = false;
        let mut body = pattern;
        if let Some(rest) = body.strip_prefix('!') {
            negated = true;
            body = rest;
        }
        let mut dir_only = false;
        if body.ends_with('/') {
            dir_only = true;
            body = &body[..body.len() - 1];
        }
        let mut anchored = false;
        if let Some(rest) = body.strip_prefix('/') {
            anchored = true;
            body = rest;
        }
        if body.is_empty() {
            return None;
        }
        let regex_str = glob_to_regex(body);
        let re = regex::Regex::new(&regex_str).ok()?;
        Some(IgnoreRule { negated, dir_only, anchored, regex: re })
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() * 2 + 8);
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // ** — any path segment run
                    out.push_str(".*");
                    // consume following slashes
                    i += 2;
                    while i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                    continue;
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                if c == '\\' && i + 1 < chars.len() {
                    // escaped character in pattern (e.g. \#)
                    out.push(chars[i + 1]);
                    i += 1;
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    format!("^{out}$")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-agent-skills-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn loads_declared_skills_recursively() {
        let dir = tmpdir("declared");
        write(&dir.join("a/SKILL.md"), "---\nname: a\ndescription: Skill A\n---\nBody A\n");
        write(&dir.join("b/nested/SKILL.md"), "---\nname: nested\ndescription: Skill N\n---\nBody N\n");
        write(&dir.join("c/SKILL.md"), "---\nname: c\ndescription: Skill C\n---\nBody C\n");
        write(&dir.join("c/extra.md"), "---\ndescription: Root inline\n---\nSkip me\n");
        let (skills, diagnostics) = load_skills(&dir.to_string_lossy(), &[dir.to_string_lossy().into_owned()]);
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        assert_eq!(skills.len(), 3);
        // a, b/nested, c sorted by dir traversal
        assert!(skills.iter().any(|s| s.name == "a"));
        assert!(skills.iter().any(|s| s.name == "nested"));
        assert!(skills.iter().any(|s| s.name == "c"));
        assert!(!skills.iter().any(|s| s.name == "extra"), "inline md inside declared dir skipped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_root_inline_markdown_with_description() {
        let dir = tmpdir("inline");
        write(&dir.join("tip.md"), "---\nname: tip\ndescription: A tip\n---\nTip body\n");
        write(&dir.join("node_modules").join("n.md"), "---\ndescription: NM\n---\nx\n");
        write(&dir.join(".hidden").join("h.md"), "---\ndescription: H\n---\nx\n");
        write(&dir.join("nodesc.md"), "no frontmatter\n");
        let (skills, diagnostics) = load_skills(&dir.to_string_lossy(), &[dir.to_string_lossy().into_owned()]);
        // Upstream reports the name-vs-parentDir mismatch diagnostic for
        // inline files whose frontmatter name differs from the root dir, but
        // still loads the skill.
        assert!(diagnostics.iter().any(|d| d.code == "invalid_metadata"));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "tip");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignore_files_exclude_skills() {
        let dir = tmpdir("ignore");
        write(&dir.join(".gitignore"), "skipme/\n");
        write(&dir.join("keep/SKILL.md"), "---\nname: keep\ndescription: K\n---\nK\n");
        write(&dir.join("skipme/SKILL.md"), "---\nname: skipme\ndescription: S\n---\nS\n");
        let (skills, diagnostics) = load_skills(&dir.to_string_lossy(), &[dir.to_string_lossy().into_owned()]);
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "keep");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_metadata_is_diagnosed() {
        let dir = tmpdir("invalid");
        write(&dir.join("BAD/SKILL.md"), "---\nname: Bad_Name\ndescription: Skill\n---\nB\n");
        write(&dir.join("nodesc/SKILL.md"), "---\nname: nodesc\n---\nno desc\n");
        let (skills, diagnostics) = load_skills(&dir.to_string_lossy(), &[dir.to_string_lossy().into_owned()]);
        assert_eq!(skills.len(), 1, "nodesc (missing description) produces no skill");
        assert_eq!(skills[0].name, "Bad_Name");
        assert!(!diagnostics.is_empty());
        assert!(diagnostics.iter().any(|d| d.code == "invalid_metadata"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_invocation_includes_location_and_instructions() {
        let skill = Skill {
            name: "s".into(),
            description: "d".into(),
            content: "instructions".into(),
            file_path: "/skills/s/SKILL.md".into(),
            disable_model_invocation: false,
        };
        let out = format_skill_invocation(&skill, Some("extra"));
        assert!(out.contains("<skill name=\"s\" location=\"/skills/s/SKILL.md\">"));
        assert!(out.contains("References are relative to /skills/s."));
        assert!(out.contains("instructions"));
        assert!(out.ends_with("extra"));
    }

    #[test]
    fn ignore_matcher_semantics() {
        let mut m = IgnoreMatcher::default();
        m.add("build/");
        assert!(m.ignores("build/"));
        assert!(!m.ignores("build"));
        // Unanchored directory patterns match at any level (gitignore).
        assert!(m.ignores("src/build/"));
        let mut m2 = IgnoreMatcher::default();
        m2.add("*.log");
        assert!(m2.ignores("x.log"));
        assert!(m2.ignores("a/b/c.log"));
        assert!(!m2.ignores("x.txt"));
        let mut m3 = IgnoreMatcher::default();
        m3.add("*.log");
        m3.add("!keep.log");
        assert!(!m3.ignores("keep.log"));
        assert!(m3.ignores("drop.log"));
    }
}
