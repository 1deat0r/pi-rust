//! Project context-file loading — port of `resource-loader.ts`
//! `loadProjectContextFiles`/`findShadowedContextFile` and
//! `footer-data-provider.ts` `findGitPaths`.
//!
//! Discovers AGENTS.md / CLAUDE.md context files from the agent dir, the cwd,
//! and its ancestors (highest ancestor first), skipping a shadowed worktree
//! context file that the main repo's own copy already provides.

use std::path::{Path, PathBuf};

/// A discovered context file (AGENTS.md / CLAUDE.md and variants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

const CONTEXT_CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn strip_bom(s: &str) -> String {
    s.trim_start_matches('\u{feff}').to_string()
}

/// `loadContextFileFromDir` — first candidate file present in `dir`.
fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    for filename in CONTEXT_CANDIDATES {
        let file_path = dir.join(filename);
        if !file_path.is_file() {
            continue;
        }
        match std::fs::read_to_string(&file_path) {
            Ok(content) => {
                return Some(ContextFile {
                    path: file_path.to_string_lossy().into_owned(),
                    content: strip_bom(&content),
                });
            }
            Err(e) => {
                tracing::warn!("could not read context file {}: {e}", file_path.display());
            }
        }
    }
    None
}

/// Git metadata paths for the repository containing `cwd` (port of
/// `footer-data-provider.ts findGitPaths`). Handles both regular repos (`.git`
/// is a directory) and linked worktrees (`.git` is a `gitdir:` file).
#[derive(Debug, Clone)]
pub struct GitPaths {
    pub repo_dir: String,
    pub common_git_dir: String,
    pub head_path: String,
}

pub fn find_git_paths(cwd: &str) -> Option<GitPaths> {
    let mut dir = PathBuf::from(cwd);
    loop {
        let git_path = dir.join(".git");
        if git_path.exists() {
            let meta = match std::fs::metadata(&git_path) {
                Ok(m) => m,
                Err(_) => return None,
            };
            if meta.is_file() {
                let content = match std::fs::read_to_string(&git_path) {
                    Ok(c) => c,
                    Err(_) => return None,
                };
                let trimmed = content.trim();
                if let Some(git_dir_rel) = trimmed.strip_prefix("gitdir: ") {
                    let git_dir = dir.join(git_dir_rel.trim());
                    let head_path = git_dir.join("HEAD");
                    if !head_path.exists() {
                        return None;
                    }
                    let commondir_path = git_dir.join("commondir");
                    let common_git_dir = if commondir_path.exists() {
                        match std::fs::read_to_string(&commondir_path) {
                            Ok(c) => git_dir.join(c.trim()),
                            Err(_) => git_dir.clone(),
                        }
                    } else {
                        git_dir.clone()
                    };
                    return Some(GitPaths {
                        repo_dir: dir.to_string_lossy().into_owned(),
                        common_git_dir: common_git_dir.to_string_lossy().into_owned(),
                        head_path: head_path.to_string_lossy().into_owned(),
                    });
                }
            } else if meta.is_dir() {
                let head_path = git_path.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir.to_string_lossy().into_owned(),
                    common_git_dir: git_path.to_string_lossy().into_owned(),
                    head_path: head_path.to_string_lossy().into_owned(),
                });
            }
        }
        let parent = match dir.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }
    None
}

fn canonical(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// `findShadowedContextFile` — when the cwd is a linked worktree nested in its
/// main repo, the main repo's context copy shadows the worktree's own; both
/// occupy the same logical repo scope, so loading both would apply the context
/// twice. Returns the main-repo context file path when shadowed, else `None`.
fn find_shadowed_context_file(cwd: &str) -> Option<String> {
    let git_paths = find_git_paths(cwd)?;
    let common_git_dir = canonical(&git_paths.common_git_dir);
    let worktree_root = canonical(&git_paths.repo_dir);
    let main_repo_root = common_git_dir.parent()?;

    let sep = std::path::MAIN_SEPARATOR.to_string();
    let main_prefix = format!("{}{sep}", main_repo_root.to_string_lossy());
    if !worktree_root.to_string_lossy().starts_with(&main_prefix) {
        return None;
    }
    let main_git = canonical(&main_repo_root.join(".git").to_string_lossy());
    if main_git != common_git_dir {
        return None;
    }
    let worktree_context = load_context_file_from_dir(&worktree_root)?;
    let name = Path::new(&worktree_context.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())?;
    Some(main_repo_root.join(name).to_string_lossy().into_owned())
}

/// `loadProjectContextFiles` — global context + cwd/ancestor context files,
/// highest ancestor first, falling back correctly across worktree boundaries.
pub fn load_project_context_files(cwd: &str, agent_dir: &str) -> Vec<ContextFile> {
    let resolved_cwd = canonical(cwd);
    let resolved_agent_dir = canonical(agent_dir);

    let mut context_files: Vec<ContextFile> = Vec::new();
    let mut seen_paths: Vec<PathBuf> = Vec::new();

    let global_context = load_context_file_from_dir(&resolved_agent_dir);
    if let Some(ctx) = global_context {
        context_files.push(ctx.clone());
        seen_paths.push(PathBuf::from(&ctx.path));
    }

    let mut ancestor_context_files: Vec<ContextFile> = Vec::new();
    let shadowed = find_shadowed_context_file(&resolved_cwd.to_string_lossy());

    let mut current_dir = resolved_cwd.clone();
    loop {
        let context_file = load_context_file_from_dir(&current_dir);
        let is_shadowed = shadowed
            .as_ref()
            .map(|s| {
                context_file
                    .as_ref()
                    .map(|c| canonical(&c.path) == canonical(s))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if let Some(ctx) = context_file {
            if !is_shadowed
                && !seen_paths
                    .iter()
                    .any(|p| p.as_path() == std::path::Path::new(&ctx.path))
            {
                ancestor_context_files.insert(0, ctx.clone());
                seen_paths.push(PathBuf::from(&ctx.path));
            }
        }
        let parent_dir = match current_dir.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
        if parent_dir == current_dir {
            break;
        }
        current_dir = parent_dir;
    }

    context_files.extend(ancestor_context_files);
    context_files
}

/// Render the `<project_context>` system-prompt section (upstream
/// `system-prompt.ts`). Returns an empty string when there are no files.
pub fn format_project_context(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out =
        "\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n".to_string();
    for f in files {
        out.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            f.path, f.content
        ));
    }
    out.push_str("</project_context>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-context-{}-{}", std::process::id(), name));
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
    fn loads_agent_dir_and_ancestor_context_files() {
        let root = tmpdir("ancestors");
        let agent = root.join("agent");
        let cwd = root.join("proj").join("nested");
        std::fs::create_dir_all(&cwd).unwrap();
        write(&agent.join("AGENTS.md"), "global context");
        write(&root.join("proj").join("AGENTS.md"), "proj context");
        write(&cwd.join("CLAUDE.md"), "nested context");

        let files = load_project_context_files(&cwd.to_string_lossy(), &agent.to_string_lossy());
        // Order: global (agentDir) first, then ancestors highest-first (proj before nested).
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].content, "global context");
        assert_eq!(files[1].content, "proj context");
        assert_eq!(files[2].content, "nested context");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bom_is_stripped_and_md_variants_recognized() {
        let root = tmpdir("variant");
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        write(&cwd.join("AGENTS.MD"), "\u{feff}uppercase variant");
        let files = load_project_context_files(
            &cwd.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "uppercase variant");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_context_files_means_empty() {
        let root = tmpdir("empty");
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let files = load_project_context_files(
            &cwd.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
        );
        assert!(files.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn find_git_paths_in_plain_repo() {
        let root = tmpdir("gitrepo");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let cwd = repo.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        let paths = find_git_paths(&cwd.to_string_lossy()).expect("git paths");
        assert!(paths.repo_dir.ends_with("repo"));
        assert!(paths.common_git_dir.ends_with(".git"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn load_context_file_strips_bom_in_header_too() {
        let root = tmpdir("override");
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        // override wins over AGENTS.md
        write(&cwd.join("AGENTS.md"), "base");
        write(&cwd.join("AGENTS.override.md"), "override");
        let files = load_project_context_files(
            &cwd.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "override");
        // only one loaded for the dir
        assert!(files[0].path.ends_with("AGENTS.override.md"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn format_project_context_wraps_files_in_xml() {
        assert_eq!(format_project_context(&[]), "");
        let files = vec![
            ContextFile {
                path: "/p/AGENTS.md".into(),
                content: "be concise".into(),
            },
            ContextFile {
                path: "/p/nested/CLAUDE.md".into(),
                content: "note".into(),
            },
        ];
        let out = format_project_context(&files);
        assert!(out.starts_with("\n\n<project_context>"));
        assert!(out.ends_with("</project_context>\n"));
        assert!(out.contains(
            "<project_instructions path=\"/p/AGENTS.md\">\nbe concise\n</project_instructions>"
        ));
        assert!(out.contains("Project-specific instructions and guidelines:"));
    }
}
