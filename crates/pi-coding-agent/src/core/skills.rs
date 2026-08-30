//! Skill loader — port of `packages/coding-agent/src/core/skills.ts`.
//!
//! Discovers skills from the user skills dir (`<agentDir>/skills`), the
//! project skills dir (`<cwd>/.pi/skills`), and explicit `--skill` paths,
//! validates name/description per the Agent Skills spec, and renders the
//! `<available_skills>` block for the system prompt. Distinct from the
//! agent-harness skill loader (`pi-agent::harness::skills`), which loads
//! skills into the harness resources at a lower layer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pi_agent::harness::frontmatter::parse_frontmatter;
use pi_agent::harness::skills::IgnoreMatcher;

use crate::config::CONFIG_DIR_NAME;
use crate::core::diagnostics::{ResourceDiagnostic, ResourceDiagnosticKind};
use crate::core::extensions::types::SourceInfo;

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// A loaded skill (coding-agent surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
}

pub type LoadSkillsResult = (Vec<Skill>, Vec<ResourceDiagnostic>);

/// Options for `load_skills`.
pub struct LoadSkillsOptions {
    pub cwd: String,
    pub agent_dir: String,
    /// Explicit skill paths (files or directories) from `--skill`.
    pub skill_paths: Vec<String>,
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn to_posix(p: &str) -> String {
    p.replace('\\', "/")
}

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

fn add_ignore_rules(ig: &mut IgnoreMatcher, dir: &Path, root_dir: &Path) {
    let rel = relative_posix(root_dir, dir);
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        let Ok(content) = std::fs::read_to_string(&ignore_path) else {
            continue;
        };
        for line in content.split('\n') {
            let pattern = prefix_ignore_pattern(line.trim_end_matches('\r'), &prefix);
            if let Some(p) = pattern {
                ig.add(&p);
            }
        }
    }
}

fn relative_posix(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => to_posix(&rel.to_string_lossy()),
        Err(_) => to_posix(&path.to_string_lossy()),
    }
}

struct DirOutcome {
    skills: Vec<Skill>,
    diagnostics: Vec<ResourceDiagnostic>,
}

fn load_skills_from_dir_internal(
    dir: &Path,
    include_root_files: bool,
    matcher: &mut IgnoreMatcher,
    root_dir: &Path,
    source: &str,
) -> DirOutcome {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    if !dir.is_dir() {
        return DirOutcome {
            skills,
            diagnostics,
        };
    }

    add_ignore_rules(matcher, dir, root_dir);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            return DirOutcome {
                skills,
                diagnostics,
            }
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }

    // A declared skill directory: a regular SKILL.md short-circuits. Match
    // upstream's `Dirent.isFile()` check so malformed entries (for example a
    // directory named SKILL.md or a broken symlink) do not hide other skills.
    if names.iter().any(|n| {
        n == "SKILL.md"
            && std::fs::metadata(dir.join(n))
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
    }) {
        let full_path = dir.join("SKILL.md");
        let rel = relative_posix(root_dir, &full_path);
        if !matcher.ignores(&rel) {
            let result = load_skill_from_file(&full_path, source);
            if let Some(skill) = result.skill {
                skills.push(skill);
            }
            diagnostics.extend(result.diagnostics);
        }
        return DirOutcome {
            skills,
            diagnostics,
        };
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
        let Ok(metadata) = std::fs::metadata(&full_path) else {
            // Upstream silently skips entries whose type cannot be resolved,
            // including broken symlinks.
            continue;
        };
        let is_dir = metadata.is_dir();
        let rel = relative_posix(root_dir, &full_path);
        let ignore_path = if is_dir {
            format!("{rel}/")
        } else {
            rel.clone()
        };
        if matcher.ignores(&ignore_path) {
            continue;
        }
        if is_dir {
            let mut out =
                load_skills_from_dir_internal(&full_path, false, matcher, root_dir, source);
            skills.append(&mut out.skills);
            diagnostics.append(&mut out.diagnostics);
            continue;
        }
        if !include_root_files || !name.ends_with(".md") {
            continue;
        }
        let result = load_skill_from_file(&full_path, source);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }
    DirOutcome {
        skills,
        diagnostics,
    }
}

/// Load skills from a single directory (recursive).
pub fn load_skills_from_dir(dir: &str) -> LoadSkillsResult {
    let mut matcher = IgnoreMatcher::default();
    let root = Path::new(dir).to_path_buf();
    let out = load_skills_from_dir_internal(Path::new(dir), true, &mut matcher, &root, "path");
    (out.skills, out.diagnostics)
}

fn load_skill_from_file(file_path: &Path, source: &str) -> SkillFileOutcome {
    let mut diagnostics = Vec::new();
    let is_declared_skill = file_path
        .file_name()
        .map(|n| n == "SKILL.md")
        .unwrap_or(false);

    let raw = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            diagnostics.push(ResourceDiagnostic::warning(
                e.to_string(),
                path_to_string(file_path),
            ));
            return SkillFileOutcome {
                skill: None,
                diagnostics,
            };
        }
    };

    // The upstream YAML parser strips a UTF-8 BOM before looking for the
    // opening fence.  `pi-agent`'s shared parser intentionally stays generic
    // and does not do that normalization, so perform it at this coding-agent
    // boundary just as the TypeScript resource loader does.
    let raw = crate::core::settings::strip_bom(&raw);
    let Some((frontmatter, _body)) = parse_frontmatter(raw) else {
        if is_declared_skill {
            diagnostics.push(ResourceDiagnostic::warning(
                "failed to parse skill file",
                path_to_string(file_path),
            ));
        }
        return SkillFileOutcome {
            skill: None,
            diagnostics,
        };
    };

    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let has_description = description
        .as_deref()
        .map(|d| !d.trim().is_empty())
        .unwrap_or(false);
    if !is_declared_skill && !has_description {
        return SkillFileOutcome {
            skill: None,
            diagnostics,
        };
    }

    let skill_dir = file_path.parent().map(path_to_string).unwrap_or_default();
    let parent_dir_name = file_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    for error in validate_description(description.as_deref()) {
        diagnostics.push(ResourceDiagnostic::warning(
            error,
            path_to_string(file_path),
        ));
    }

    let frontmatter_name = frontmatter.get("name").and_then(|v| v.as_str());
    let name = frontmatter_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent_dir_name.clone());
    for error in validate_name(&name) {
        diagnostics.push(ResourceDiagnostic::warning(
            error,
            path_to_string(file_path),
        ));
    }

    let Some(description) = description else {
        return SkillFileOutcome {
            skill: None,
            diagnostics,
        };
    };
    if description.trim().is_empty() {
        return SkillFileOutcome {
            skill: None,
            diagnostics,
        };
    }

    SkillFileOutcome {
        skill: Some(Skill {
            name,
            description,
            file_path: path_to_string(file_path),
            base_dir: skill_dir.clone(),
            source_info: skill_source_info(file_path, &skill_dir, source),
            disable_model_invocation: frontmatter
                .get("disable-model-invocation")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        diagnostics,
    }
}

fn skill_source_info(file_path: &Path, base_dir: &str, source: &str) -> SourceInfo {
    let mut info = match source {
        "user" | "project" | "path" => SourceInfo::synthetic(
            &path_to_string(file_path),
            "local",
            Some(base_dir.to_string()),
        ),
        other => SourceInfo::synthetic(
            &path_to_string(file_path),
            other,
            Some(base_dir.to_string()),
        ),
    };
    if source == "user" || source == "project" {
        info.scope = source.to_string();
    }
    info
}

struct SkillFileOutcome {
    skill: Option<Skill>,
    diagnostics: Vec<ResourceDiagnostic>,
}

fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
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
            errors.push(format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                d.chars().count()
            ));
        }
        _ => {}
    }
    errors
}

/// Escape XML special characters.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the `<available_skills>` block for the system prompt (upstream
/// `formatSkillsForPrompt`). Skills with `disable_model_invocation` are
/// excluded (they can only be invoked explicitly).
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in &visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn resolve_path(path: &str, cwd: &str) -> PathBuf {
    let expanded = crate::config::expand_tilde_path(path.trim());
    let p = Path::new(&expanded);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    };
    // Match upstream `resolvePath`: lexical `.`/`..` segments are removed
    // before discovery so resource paths and source metadata cannot retain a
    // caller's alternate spelling of the same file.
    crate::core::settings::resolve_path(&joined.to_string_lossy())
}

/// Load skills from all configured locations (user, project, explicit).
fn add_loaded_skill(
    skill: Skill,
    skill_map: &mut Vec<(String, Skill)>,
    real_path_set: &mut HashSet<String>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    let real_path = std::fs::canonicalize(&skill.file_path)
        .unwrap_or_else(|_| PathBuf::from(&skill.file_path))
        .to_string_lossy()
        .into_owned();
    if real_path_set.contains(&real_path) {
        return;
    }
    if let Some((_, existing)) = skill_map.iter().find(|(n, _)| *n == skill.name) {
        diagnostics.push(collision_diagnostic(&skill, existing));
    } else {
        real_path_set.insert(real_path);
        skill_map.push((skill.name.clone(), skill));
    }
}

pub fn load_skills(options: LoadSkillsOptions) -> LoadSkillsResult {
    load_skills_with_defaults(options, true)
}

/// Load only the supplied skill paths, preserving the upstream `--no-skills`
/// contract: automatic user/project discovery is disabled, but explicit CLI
/// and extension resource paths remain active.
pub fn load_skills_without_defaults(options: LoadSkillsOptions) -> LoadSkillsResult {
    load_skills_with_defaults(options, false)
}

fn load_skills_with_defaults(
    options: LoadSkillsOptions,
    include_defaults: bool,
) -> LoadSkillsResult {
    // The upstream loader resolves cwd/agentDir before deriving default
    // roots; this also keeps relative or `~` paths consistent with explicit
    // `--skill` inputs.
    let resolved_cwd = crate::core::settings::resolve_path(&options.cwd);
    let resolved_agent_dir = crate::core::settings::resolve_path(&options.agent_dir);
    let agent_skills_dir = resolved_agent_dir.join("skills");
    let project_skills_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("skills");

    let mut skill_map: Vec<(String, Skill)> = Vec::new();
    let mut real_path_set = HashSet::new();
    let mut all_diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    if include_defaults {
        // Defaults: user + project dirs. Each upstream loadSkillsFromDir call
        // owns its ignore matcher; sharing one would leak root-relative rules
        // between scopes.
        let mut add_dir = |dir: &Path, source: &str, diagnostics: &mut Vec<ResourceDiagnostic>| {
            let mut matcher = IgnoreMatcher::default();
            let out = load_skills_from_dir_internal(dir, true, &mut matcher, dir, source);
            for skill in out.skills {
                add_loaded_skill(skill, &mut skill_map, &mut real_path_set, diagnostics);
            }
            diagnostics.extend(out.diagnostics);
        };
        add_dir(&agent_skills_dir, "user", &mut all_diagnostics);
        add_dir(&project_skills_dir, "project", &mut all_diagnostics);
    }

    // Explicit `--skill` paths.
    for raw_path in &options.skill_paths {
        let resolved = resolve_path(raw_path, resolved_cwd.to_string_lossy().as_ref());
        if !resolved.exists() {
            all_diagnostics.push(ResourceDiagnostic::warning(
                "skill path does not exist",
                path_to_string(&resolved),
            ));
            continue;
        }
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(e) => {
                all_diagnostics.push(ResourceDiagnostic::warning(
                    e.to_string(),
                    path_to_string(&resolved),
                ));
                continue;
            }
        };
        if meta.is_dir() {
            let mut matcher = IgnoreMatcher::default();
            let out =
                load_skills_from_dir_internal(&resolved, true, &mut matcher, &resolved, "path");
            for skill in out.skills {
                add_loaded_skill(
                    skill,
                    &mut skill_map,
                    &mut real_path_set,
                    &mut all_diagnostics,
                );
            }
            all_diagnostics.extend(out.diagnostics);
        } else if meta.is_file() && resolved.to_string_lossy().ends_with(".md") {
            let result = load_skill_from_file(&resolved, "path");
            if let Some(skill) = result.skill {
                add_loaded_skill(
                    skill,
                    &mut skill_map,
                    &mut real_path_set,
                    &mut all_diagnostics,
                );
            }
            all_diagnostics.extend(result.diagnostics);
        } else {
            all_diagnostics.push(ResourceDiagnostic::warning(
                "skill path is not a markdown file",
                path_to_string(&resolved),
            ));
        }
    }

    (
        skill_map.into_iter().map(|(_, s)| s).collect(),
        all_diagnostics,
    )
}

fn collision_diagnostic(loser: &Skill, winner: &Skill) -> ResourceDiagnostic {
    ResourceDiagnostic {
        kind: ResourceDiagnosticKind::Collision,
        message: format!("name \"{}\" collision", loser.name),
        path: Some(loser.file_path.clone()),
        collision: Some(crate::core::diagnostics::ResourceCollision {
            resource_type: "skill",
            name: loser.name.clone(),
            winner_path: winner.file_path.clone(),
            loser_path: loser.file_path.clone(),
            winner_source: None,
            loser_source: None,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-skills-{}-{}", std::process::id(), name));
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
    fn loads_declared_skills_recursively_from_dir() {
        let dir = tmpdir("declared");
        write(
            &dir.join("a/SKILL.md"),
            "---\nname: a\ndescription: Skill A\n---\nBody A\n",
        );
        write(
            &dir.join("b/SKILL.md"),
            "---\nname: b\ndescription: Skill B\n---\nBody B\n",
        );
        write(&dir.join("a/extra.md"), "---\ndescription: skip\n---\nx\n");
        let (skills, diagnostics) = load_skills_from_dir(&dir.to_string_lossy());
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        assert_eq!(skills.len(), 2);
        assert!(skills.iter().any(|s| s.name == "a"));
        assert!(skills.iter().any(|s| s.name == "b"));
        assert!(skills.iter().all(|skill| {
            skill.source_info.source == "local"
                && skill
                    .source_info
                    .base_dir
                    .as_deref()
                    .is_some_and(|base| base == skill.base_dir)
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_file_skill_marker_does_not_hide_other_skills() {
        let dir = tmpdir("marker-directory");
        std::fs::create_dir_all(dir.join("SKILL.md/nested")).unwrap();
        write(
            &dir.join("SKILL.md/nested/SKILL.md"),
            "---\nname: nested\ndescription: Nested skill\n---\nBody\n",
        );
        write(
            &dir.join("root.md"),
            "---\nname: root\ndescription: Root skill\n---\nBody\n",
        );

        let (skills, diagnostics) = load_skills_from_dir(&dir.to_string_lossy());
        let names: HashSet<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, HashSet::from(["nested", "root"]));
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_root_inline_markdown_with_description() {
        let dir = tmpdir("inline");
        write(
            &dir.join("tip.md"),
            "---\nname: tip\ndescription: A tip\n---\nTip body\n",
        );
        write(&dir.join("nodesc.md"), "no frontmatter\n");
        let (skills, _) = load_skills_from_dir(&dir.to_string_lossy());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "tip");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_declared_skill_frontmatter_with_utf8_bom() {
        let dir = tmpdir("bom");
        write(
            &dir.join("bom-skill/SKILL.md"),
            "\u{feff}---\nname: bom-skill\ndescription: BOM-safe skill\n---\nBody\n",
        );

        let (skills, diagnostics) = load_skills_from_dir(&dir.to_string_lossy());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "bom-skill");
        assert_eq!(skills[0].description, "BOM-safe skill");
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn name_from_parent_dir_when_frontmatter_name_missing() {
        let dir = tmpdir("paren");
        write(
            &dir.join("my-skill/SKILL.md"),
            "---\ndescription: Skill P\n---\nBody\n",
        );
        let (skills, _) = load_skills_from_dir(&dir.to_string_lossy());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_names_and_descriptions_are_diagnosed() {
        let dir = tmpdir("invalid");
        write(
            &dir.join("BAD/SKILL.md"),
            "---\nname: Bad_Name\ndescription: d\n---\nb\n",
        );
        write(
            &dir.join("nodesc/SKILL.md"),
            "---\nname: nodesc\n---\nno desc\n",
        );
        let (skills, diagnostics) = load_skills_from_dir(&dir.to_string_lossy());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "Bad_Name");
        assert!(diagnostics
            .iter()
            .any(|d| d.message.contains("invalid characters")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignore_files_exclude_skills() {
        let dir = tmpdir("ign");
        write(&dir.join(".gitignore"), "skipme/\n");
        write(
            &dir.join("keep/SKILL.md"),
            "---\nname: keep\ndescription: K\n---\nK\n",
        );
        write(
            &dir.join("skipme/SKILL.md"),
            "---\nname: skipme\ndescription: S\n---\nS\n",
        );
        let (skills, diagnostics) = load_skills_from_dir(&dir.to_string_lossy());
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "keep");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_skills_for_prompt_renders_available_skills_block() {
        let skills = vec![
            Skill {
                name: "A&B".into(),
                description: "desc".into(),
                file_path: "/s/a/SKILL.md".into(),
                base_dir: "/s/a".into(),
                source_info: SourceInfo::synthetic("/s/a/SKILL.md", "test", Some("/s/a".into())),
                disable_model_invocation: false,
            },
            Skill {
                name: "hidden".into(),
                description: "h".into(),
                file_path: "/s/h/SKILL.md".into(),
                base_dir: "/s/h".into(),
                source_info: SourceInfo::synthetic("/s/h/SKILL.md", "test", Some("/s/h".into())),
                disable_model_invocation: true,
            },
        ];
        let out = format_skills_for_prompt(&skills);
        assert!(out.contains("<available_skills>"));
        assert!(out.contains("<name>A&amp;B</name>"));
        assert!(out.contains("<location>/s/a/SKILL.md</location>"));
        assert!(!out.contains("hidden"));
        assert!(
            !out.contains("<skill name="),
            "uses available_skills XML, not invocation blocks"
        );
    }

    #[test]
    fn empty_or_disabled_skills_produce_empty_prompt_block() {
        assert_eq!(format_skills_for_prompt(&[]), "");
    }

    #[test]
    fn load_skills_dedups_by_name_with_collision() {
        let user = tmpdir("user");
        let project = tmpdir("project");
        write(
            &user.join("skills/dup/SKILL.md"),
            "---\nname: dup\ndescription: user\n---\nu\n",
        );
        write(
            &project.join(".pi/skills/dup/SKILL.md"),
            "---\nname: dup\ndescription: project\n---\np\n",
        );
        let (skills, diagnostics) = load_skills(LoadSkillsOptions {
            cwd: project.to_string_lossy().into_owned(),
            agent_dir: user.to_string_lossy().into_owned(),
            skill_paths: vec![],
        });
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "user", "user wins over project");
        assert!(diagnostics.iter().any(|d| {
            d.kind == ResourceDiagnosticKind::Collision
                && matches!(&d.collision, Some(c) if c.name == "dup")
        }));
        std::fs::remove_dir_all(&user).ok();
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn explicit_skill_path_loaded_as_file() {
        let dir = tmpdir("explicit");
        write(
            &dir.join("custom/SKILL.md"),
            "---\nname: custom\ndescription: C\n---\nC\n",
        );
        let (skills, _) = load_skills(LoadSkillsOptions {
            cwd: dir.to_string_lossy().into_owned(),
            agent_dir: dir.join("agent").to_string_lossy().into_owned(),
            skill_paths: vec![dir.join("custom").to_string_lossy().into_owned()],
        });
        assert!(skills.iter().any(|s| s.name == "custom"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_skill_path_uses_lexically_normalized_provenance() {
        let dir = tmpdir("explicit-normalized");
        let skill = dir.join("custom").join("SKILL.md");
        write(&skill, "---\nname: custom\ndescription: C\n---\nC\n");
        let spelling = dir
            .join("custom")
            .join("nested")
            .join("..")
            .join("SKILL.md");

        let (skills, diagnostics) = load_skills(LoadSkillsOptions {
            cwd: dir.to_string_lossy().into_owned(),
            agent_dir: dir.join("agent").to_string_lossy().into_owned(),
            skill_paths: vec![spelling.to_string_lossy().into_owned()],
        });
        assert_eq!(skills.len(), 1);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert_eq!(skills[0].file_path, skill.to_string_lossy().into_owned());
        assert!(!skills[0].file_path.contains(".."));
        std::fs::remove_dir_all(&dir).ok();
    }
}
