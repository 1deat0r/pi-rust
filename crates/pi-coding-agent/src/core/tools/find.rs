//! `find` tool — port of `packages/coding-agent/src/core/tools/find.ts`.
//!
//! Globs files with the `fd` binary using the exact upstream argument set
//! (`--glob --color=never --hidden [--no-require-git] --max-results N
//! [--full-path] -- <pattern> <searchPath>`), relativizes results to the
//! search root, and emits the upstream notices.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use pi_agent::tools::path_utils::resolve_tool_path;
use pi_agent::tools::truncate::{truncate_head_with, DEFAULT_MAX_BYTES};
use pi_agent::tools::{AgentTool, AgentToolResult};
use pi_ai::types::json_tool;

use super::{bytes_limit_notice, truncation_details, wait_for_abort, ToolOutput};

const DEFAULT_LIMIT: u64 = 1000;

/// Node `path.relative` for the (absolute, same-volume) cases fd emits.
pub(crate) fn path_relative(from: &Path, to: &Path) -> String {
    let from_comps: Vec<&OsStr> = from.components().map(|c| c.as_os_str()).collect();
    let to_comps: Vec<&OsStr> = to.components().map(|c| c.as_os_str()).collect();
    let mut i = 0;
    while i < from_comps.len() && i < to_comps.len() && from_comps[i] == to_comps[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..from_comps.len() {
        parts.push("..".to_string());
    }
    for c in &to_comps[i..] {
        parts.push(c.to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("/")
    }
}

/// Upstream `relativizeFindResultPath`: relativize against the search root and
/// normalize to posix separators, preserving a trailing directory separator.
fn relativize_find_result_path(result_path: &str, search_path: &str) -> String {
    let had_trailing_separator = result_path.ends_with('/') || result_path.ends_with('\\');
    let relative = if Path::new(result_path).is_absolute() {
        path_relative(Path::new(search_path), Path::new(result_path))
    } else {
        result_path.to_string()
    };
    let relative = relative.replace('\\', "/");
    if had_trailing_separator && !relative.ends_with('/') {
        format!("{relative}/")
    } else {
        relative
    }
}

/// True when any ancestor of `dir` (including itself) contains `.git`.
fn inside_git_repo(search_path: &str) -> bool {
    let mut current = Path::new(search_path).to_path_buf();
    loop {
        if current.join(".git").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

/// Model-facing execute: returns the text output for `find`.
pub async fn find_execute(
    cwd: &str,
    pattern: &str,
    path: Option<&str>,
    limit: Option<u64>,
) -> Result<String, String> {
    Ok(
        find_execute_with_details(cwd, pattern, path, limit.map(|value| value as f64), None)
            .await?
            .text,
    )
}

async fn find_execute_with_details(
    cwd: &str,
    pattern: &str,
    path: Option<&str>,
    limit: Option<f64>,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<ToolOutput, String> {
    let search_path =
        crate::core::settings::normalize_path(resolve_tool_path(cwd, path.unwrap_or(".")).into())
            .to_string_lossy()
            .into_owned();
    let effective_limit = limit.unwrap_or(DEFAULT_LIMIT as f64);

    let mut args: Vec<String> = vec![
        "--glob".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
    ];
    if !inside_git_repo(&search_path) {
        args.push("--no-require-git".to_string());
    }
    args.push("--max-results".to_string());
    args.push(effective_limit.to_string());

    // Path-containing patterns match in --full-path mode and need a leading
    // `**/` unless already rooted (upstream comment + logic).
    let mut effective_pattern = pattern.to_string();
    if pattern.contains('/') {
        args.push("--full-path".to_string());
        if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
            effective_pattern = format!("**/{pattern}");
        }
        if cfg!(windows) {
            effective_pattern = effective_pattern.replace('/', r"[/\\]");
        }
    }
    args.push("--".to_string());
    args.push(effective_pattern);
    args.push(search_path.clone());

    if pi_agent::agent::is_aborted(signal.as_ref()) {
        return Err("Operation aborted".to_string());
    }
    let mut command = tokio::process::Command::new("fd");
    command
        .args(&args)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = if let Some(signal) = signal {
        tokio::select! {
            output = command.output() => output,
            _ = wait_for_abort(signal) => return Err("Operation aborted".to_string()),
        }
    } else {
        command.output().await
    }
    .map_err(map_fd_spawn_error)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() && stdout.trim().is_empty() {
        let msg = if stderr.trim().is_empty() {
            format!("fd exited with code {}", output.status.code().unwrap_or(-1))
        } else {
            stderr.trim().to_string()
        };
        return Err(msg);
    }

    let relativized: Vec<String> = stdout
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .map(|l| relativize_find_result_path(&l, &search_path))
        .collect();

    if relativized.is_empty() {
        return Ok(ToolOutput {
            text: "No files found matching pattern".to_string(),
            details: None,
        });
    }

    let result_limit_reached = (relativized.len() as f64) >= effective_limit;
    let raw_output = relativized.join("\n");
    let truncation = truncate_head_with(&raw_output, 1_000_000, DEFAULT_MAX_BYTES);
    let mut result_output = truncation.content.clone();
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        notices.push(format!(
            "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2.0
        ));
    }
    if truncation.truncated {
        notices.push(bytes_limit_notice());
    }
    if !notices.is_empty() {
        result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let mut details = serde_json::Map::new();
    if result_limit_reached {
        details.insert(
            "resultLimitReached".to_string(),
            json_limit(effective_limit),
        );
    }
    if truncation.truncated {
        details.insert(
            "truncation".to_string(),
            truncation_details(&truncation, raw_output.len(), 1_000_000),
        );
    }
    Ok(ToolOutput {
        text: result_output,
        details: (!details.is_empty()).then_some(serde_json::Value::Object(details)),
    })
}

/// Builds the `find` tool bound to a working directory.
pub fn find_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "find",
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
                    "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                    "limit": {"type": "number", "description": "Maximum number of results (default: 1000)"}
                },
                "required": ["pattern"]
            }),
        ),
        "Find files",
        Arc::new(move |_tool_call_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if pi_agent::agent::is_aborted(signal.as_ref()) {
                    return Err("Operation aborted".to_string());
                }
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "find: missing required argument pattern".to_string())?;
                let path = args.get("path").and_then(|v| v.as_str());
                let limit = args.get("limit").and_then(|v| v.as_f64());
                match find_execute_with_details(&cwd, pattern, path, limit, signal.clone()).await {
                    Ok(_output) if pi_agent::agent::is_aborted(signal.as_ref()) => {
                        Err("Operation aborted".to_string())
                    }
                    Ok(output) => Ok(AgentToolResult {
                        content: vec![pi_ai::types::ContentBlock::text(output.text)],
                        details: output.details,
                        ..AgentToolResult::default()
                    }),
                    Err(e) => Err(e),
                }
            })
        }),
    )
    .with_experimental_sampling()
}

fn map_fd_spawn_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "fd is not available and could not be downloaded".to_string()
    } else {
        format!("Failed to run fd: {error}")
    }
}

fn json_limit(limit: f64) -> serde_json::Value {
    if limit >= 0.0 && limit.fract() == 0.0 && limit <= u64::MAX as f64 {
        serde_json::Value::from(limit as u64)
    } else {
        serde_json::json!(limit)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    struct Tree {
        root: std::path::PathBuf,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("pi-find-{tag}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("src").join("sub")).unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();
            fs::create_dir_all(root.join("ignored")).unwrap();
            fs::write(root.join("Cargo.toml"), "").unwrap();
            fs::write(root.join("src").join("main.rs"), "").unwrap();
            fs::write(root.join("src").join("lib.rs"), "").unwrap();
            fs::write(root.join("src").join("sub").join("deep.rs"), "").unwrap();
            fs::write(root.join("vendor").join("dep.rs"), "").unwrap();
            fs::write(root.join("ignored").join("skip.rs"), "").unwrap();
            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn sorted_lines(out: &str) -> Vec<String> {
        let mut v: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn globs_files_relative_to_search_root() {
        let tree = Tree::new("glob");
        let out = find_execute(&tree.root.display().to_string(), "*.rs", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert_eq!(
            lines,
            vec![
                "ignored/skip.rs",
                "src/lib.rs",
                "src/main.rs",
                "src/sub/deep.rs",
                "vendor/dep.rs"
            ]
        );
    }

    #[tokio::test]
    async fn nested_pattern_uses_full_path_mode() {
        let tree = Tree::new("nested");
        let out = find_execute(&tree.root.display().to_string(), "**/sub/*.rs", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert_eq!(lines, vec!["src/sub/deep.rs"]);
    }

    #[tokio::test]
    async fn directory_pattern_keeps_trailing_slash() {
        let tree = Tree::new("dir");
        let out = find_execute(&tree.root.display().to_string(), "src", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert_eq!(lines, vec!["src/"]);
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let tree = Tree::new("gi");
        fs::write(tree.root.join(".gitignore"), "vendor/\n").unwrap();
        let out = find_execute(&tree.root.display().to_string(), "*.rs", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert!(
            !lines.iter().any(|l| l.starts_with("vendor/")),
            "got: {lines:?}"
        );
        assert!(lines.contains(&"ignored/skip.rs".to_string()));
    }

    #[tokio::test]
    async fn nested_repository_uses_its_own_ignore_boundary() {
        let tree = Tree::new("nested-repo-ignore");
        fs::create_dir_all(tree.root.join(".git")).unwrap();
        fs::write(tree.root.join(".gitignore"), "*.generated.rs\n").unwrap();
        fs::create_dir_all(tree.root.join("nested").join(".git")).unwrap();
        fs::create_dir_all(tree.root.join("nested").join("ignored")).unwrap();
        fs::write(tree.root.join("nested").join(".gitignore"), "ignored/\n").unwrap();
        fs::write(tree.root.join("nested").join("keep.generated.rs"), "").unwrap();
        fs::write(tree.root.join("nested").join("ignored").join("drop.rs"), "").unwrap();

        let out = find_execute(&tree.root.display().to_string(), "*.rs", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert!(lines.contains(&"nested/keep.generated.rs".to_string()));
        assert!(!lines.contains(&"nested/ignored/drop.rs".to_string()));
    }

    #[tokio::test]
    async fn no_matches_returns_marker() {
        let tree = Tree::new("nomatch");
        let out = find_execute(&tree.root.display().to_string(), "*.zzz", None, None)
            .await
            .unwrap();
        assert_eq!(out, "No files found matching pattern");
    }

    #[tokio::test]
    async fn limit_notice_appears() {
        let tree = Tree::new("limit");
        let out = find_execute(&tree.root.display().to_string(), "*.rs", None, Some(1))
            .await
            .unwrap();
        assert!(
            out.contains("1 results limit reached. Use limit=2 for more, or refine pattern"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn hidden_unicode_and_symlink_entries_are_returned() {
        #[cfg(unix)]
        use std::os::unix::fs::symlink;

        let tree = Tree::new("hidden-unicode-symlink");
        fs::write(tree.root.join(".秘密.rs"), "").unwrap();
        fs::write(tree.root.join("界🙂.rs"), "").unwrap();
        #[cfg(unix)]
        {
            symlink("src/main.rs", tree.root.join("file-link.rs")).unwrap();
            symlink("missing.rs", tree.root.join("dangling-link.rs")).unwrap();
        }

        let out = find_execute(&tree.root.display().to_string(), "*.rs", None, None)
            .await
            .unwrap();
        let lines = sorted_lines(&out);
        assert!(lines.contains(&".秘密.rs".to_string()));
        assert!(lines.contains(&"界🙂.rs".to_string()));
        #[cfg(unix)]
        {
            assert!(lines.contains(&"file-link.rs".to_string()));
            assert!(lines.contains(&"dangling-link.rs".to_string()));
        }
    }

    #[tokio::test]
    async fn invalid_fractional_and_negative_limits_reach_fd() {
        let tree = Tree::new("invalid-numeric-limits");
        for limit in [1.5, -1.0] {
            let error = find_execute_with_details(
                &tree.root.display().to_string(),
                "*.rs",
                None,
                Some(limit),
                None,
            )
            .await
            .unwrap_err();
            assert!(
                error.contains("--max-results")
                    && (error.contains("invalid value") || error.contains("value is required")),
                "got: {error}"
            );
        }
    }

    #[tokio::test]
    async fn invalid_glob_and_search_roots_are_actionable() {
        let tree = Tree::new("invalid-inputs");
        let glob = find_execute(&tree.root.display().to_string(), "[", None, None)
            .await
            .unwrap_err();
        assert!(glob.contains("error parsing glob"), "got: {glob}");

        let missing = find_execute(&tree.root.display().to_string(), "*", Some("missing"), None)
            .await
            .unwrap_err();
        assert!(!missing.is_empty());

        let file = find_execute(
            &tree.root.display().to_string(),
            "*",
            Some("Cargo.toml"),
            None,
        )
        .await
        .unwrap_err();
        assert!(!file.is_empty());
    }

    #[tokio::test]
    async fn pre_cancel_stops_before_spawning_fd() {
        let tree = Tree::new("pre-cancel");
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error = find_execute_with_details(
            &tree.root.display().to_string(),
            "*.rs",
            None,
            None,
            Some(signal),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Operation aborted");
    }

    #[tokio::test]
    async fn byte_limit_sets_truncation_details() {
        let root = std::env::temp_dir().join(format!("pi-find-bytes-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for index in 0..320 {
            let name = format!("{index:04}-{}.txt", "x".repeat(170));
            fs::write(root.join(name), "").unwrap();
        }

        let output = find_execute_with_details(
            &root.display().to_string(),
            "*.txt",
            None,
            Some(1000.0),
            None,
        )
        .await
        .unwrap();
        assert!(output.text.contains("50.0KB limit reached"));
        assert!(output
            .details
            .as_ref()
            .and_then(|details| details.get("truncation"))
            .is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fd_spawn_errors_distinguish_missing_from_other_failures() {
        assert_eq!(
            map_fd_spawn_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "fd is not available and could not be downloaded"
        );
        let denied = map_fd_spawn_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(denied.starts_with("Failed to run fd:"), "got: {denied}");
    }

    #[tokio::test]
    async fn absolute_result_relativize_handles_identical() {
        let tree = Tree::new("rel");
        // fd receives the absolute search path and emits absolute results;
        // relativize strips the search root.
        let rel = relativize_find_result_path(
            &tree.root.join("src").join("main.rs").display().to_string(),
            &tree.root.display().to_string(),
        );
        assert_eq!(rel, "src/main.rs");
    }

    #[test]
    fn relative_same_path_matches_node_empty_result() {
        let path = Path::new("/tmp/pi-find-root");
        assert_eq!(path_relative(path, path), "");
        assert_eq!(
            relativize_find_result_path("/tmp/pi-find-root", "/tmp/pi-find-root"),
            ""
        );
        assert_eq!(
            relativize_find_result_path("/tmp/pi-find-root/", "/tmp/pi-find-root"),
            "/"
        );
    }
}
