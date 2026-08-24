//! `grep` tool — port of `packages/coding-agent/src/core/tools/grep.ts`.
//!
//! Searches file contents with the `ripgrep` binary using the exact upstream
//! argument set (`--json --line-number --color=never --hidden [--ignore-case]
//! [--fixed-strings] [--glob G] -- <pattern> <searchPath>`), streams JSON
//! match events, formats `path:line: text` output with context support, and
//! emits the upstream truncation notices.

use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

use pi_agent::tools::path_utils::resolve_tool_path;
use pi_agent::tools::truncate::{truncate_head_with, DEFAULT_MAX_BYTES};
use pi_agent::tools::{AgentTool, AgentToolResult};
use pi_ai::types::json_tool;

use super::bytes_limit_notice;
use super::find::path_relative;

const DEFAULT_LIMIT: u64 = 100;
const GREP_MAX_LINE_LENGTH: usize = 500;

/// Upstream `truncateLine`: slice at `maxChars` + "… [truncated]".
fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.len() <= max_chars {
        (line.to_string(), false)
    } else {
        (format!("{}... [truncated]", &line[..max_chars]), true)
    }
}

/// Upstream `formatPath`: relative to the search root when it is a directory,
/// else basename of the file.
fn format_path(search_path: &str, is_directory: bool, file_path: &str) -> String {
    if is_directory {
        let relative = path_relative(Path::new(search_path), Path::new(file_path));
        if !relative.is_empty() && !relative.starts_with("..") {
            return relative;
        }
    }
    Path::new(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string())
}

struct Match {
    file_path: String,
    line_number: u64,
    line_text: Option<String>,
}

/// Model-facing execute: returns the text output for `grep`.
#[allow(clippy::too_many_arguments)] // 1:1 port of the upstream tool input surface
pub async fn grep_execute(
    cwd: &str,
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: Option<u64>,
    limit: Option<u64>,
) -> Result<String, String> {
    let search_path = resolve_tool_path(cwd, path.unwrap_or("."));

    let is_directory = match std::fs::metadata(&search_path) {
        Ok(m) => m.is_dir(),
        Err(_) => return Err(format!("Path not found: {search_path}")),
    };

    let context_value = context.filter(|c| *c > 0).unwrap_or(0) as usize;
    let effective_limit = (limit.unwrap_or(DEFAULT_LIMIT)).max(1);

    let mut args: Vec<String> = vec![
        "--json".to_string(),
        "--line-number".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
    ];
    if ignore_case {
        args.push("--ignore-case".to_string());
    }
    if literal {
        args.push("--fixed-strings".to_string());
    }
    if let Some(g) = glob {
        args.push("--glob".to_string());
        args.push(g.to_string());
    }
    args.push("--".to_string());
    args.push(pattern.to_string());
    args.push(search_path.clone());

    let mut child = tokio::process::Command::new("rg")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| "ripgrep (rg) is not available and could not be downloaded".to_string())?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "grep: failed to capture rg stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "grep: failed to capture rg stderr".to_string())?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        use tokio::io::AsyncReadExt;
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut matches: Vec<Match> = Vec::new();
    let mut match_count: u64 = 0;
    let mut match_limit_reached = false;
    let mut killed_due_to_limit = false;

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() || match_count >= effective_limit {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("match") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        match_count += 1;
        let file_path = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str());
        let line_number = data.get("line_number").and_then(|v| v.as_u64());
        if let (Some(file_path), Some(line_number)) = (file_path, line_number) {
            let line_text = data
                .get("lines")
                .and_then(|l| l.get("text"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            matches.push(Match {
                file_path: file_path.to_string(),
                line_number,
                line_text,
            });
        }
        if match_count >= effective_limit {
            match_limit_reached = true;
            killed_due_to_limit = true;
            let _ = child.kill().await;
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("rg failed to wait: {e}"))?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !killed_due_to_limit && status.code().map(|c| c != 0 && c != 1).unwrap_or(true) {
        let msg = if stderr.trim().is_empty() {
            format!("ripgrep exited with code {}", status.code().unwrap_or(-1))
        } else {
            stderr.trim().to_string()
        };
        return Err(msg);
    }

    if match_count == 0 {
        return Ok("No matches found".to_string());
    }

    // Read file lines lazily for context blocks (upstream fileCache + readFile).
    let mut file_cache: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut lines_truncated = false;
    let mut output_lines: Vec<String> = Vec::new();

    let get_file_lines = |file_path: &str,
                          cache: &mut std::collections::HashMap<String, Vec<String>>|
     -> Vec<String> {
        if let Some(lines) = cache.get(file_path) {
            return lines.clone();
        }
        let content = std::fs::read_to_string(file_path).unwrap_or_default();
        let lines: Vec<String> = content
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(|s| s.to_string())
            .collect();
        cache.insert(file_path.to_string(), lines.clone());
        lines
    };

    for m in &matches {
        let relative_path = format_path(&search_path, is_directory, &m.file_path);
        if context_value == 0 {
            if let Some(line_text) = &m.line_text {
                let sanitized = line_text
                    .replace("\r\n", "\n")
                    .replace('\r', "")
                    .trim_end_matches('\n')
                    .to_string();
                let (truncated_text, was_truncated) =
                    truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                if was_truncated {
                    lines_truncated = true;
                }
                output_lines.push(format!(
                    "{}:{}: {}",
                    relative_path, m.line_number, truncated_text
                ));
            }
        } else {
            let lines = get_file_lines(&m.file_path, &mut file_cache);
            if lines.is_empty() {
                output_lines.push(format!(
                    "{}:{}: (unable to read file)",
                    relative_path, m.line_number
                ));
                continue;
            }
            let start = context_value
                .saturating_sub(0)
                .max(1)
                .max(m.line_number.saturating_sub(context_value as u64) as usize);
            let end = (m.line_number + context_value as u64).min(lines.len() as u64) as usize;
            for current in start..=end {
                let line_text = lines.get(current - 1).cloned().unwrap_or_default();
                let sanitized = line_text.replace('\r', "");
                let (truncated_text, was_truncated) =
                    truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                if was_truncated {
                    lines_truncated = true;
                }
                if current == m.line_number as usize {
                    output_lines.push(format!("{relative_path}:{current}: {truncated_text}"));
                } else {
                    output_lines.push(format!("{relative_path}-{current}- {truncated_text}"));
                }
            }
        }
    }

    let raw_output = output_lines.join("\n");
    let truncation = truncate_head_with(&raw_output, 1_000_000, DEFAULT_MAX_BYTES);
    let mut output = truncation.content;
    let mut notices: Vec<String> = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
    }
    if truncation.truncated {
        notices.push(bytes_limit_notice());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(output)
}

/// Builds the `grep` tool bound to a working directory.
pub fn grep_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "grep",
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                    "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
                    "glob": {"type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},
                    "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
                    "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
                    "context": {"type": "number", "description": "Number of lines to show before and after each match (default: 0)"},
                    "limit": {"type": "number", "description": "Maximum number of matches to return (default: 100)"}
                },
                "required": ["pattern"]
            }),
        ),
        "Grep",
        Arc::new(move |_tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "grep: missing required argument pattern".to_string())?;
                let path = args.get("path").and_then(|v| v.as_str());
                let glob = args.get("glob").and_then(|v| v.as_str());
                let ignore_case = args.get("ignoreCase").and_then(|v| v.as_bool()).unwrap_or(false);
                let literal = args.get("literal").and_then(|v| v.as_bool()).unwrap_or(false);
                let context = args.get("context").and_then(|v| v.as_u64());
                let limit = args.get("limit").and_then(|v| v.as_u64());
                match grep_execute(&cwd, pattern, path, glob, ignore_case, literal, context, limit).await {
                    Ok(output) => Ok(AgentToolResult::output(output)),
                    Err(e) => Err(e),
                }
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tree {
        root: std::path::PathBuf,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("pi-grep-{tag}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();
            fs::write(
                root.join("src").join("main.rs"),
                "TODO: add feature\nline two\n",
            )
            .unwrap();
            fs::write(root.join("src").join("lib.rs"), "const TODO: u32 = 1;\n").unwrap();
            fs::write(root.join("vendor").join("dep.rs"), "todo lowercase todo\n").unwrap();
            fs::write(root.join("notes.md"), "TODO in markdown\n").unwrap();
            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn basic_match_outputs_path_line_text() {
        let tree = Tree::new("basic");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "TODO",
            None,
            None,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        let lines: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        assert!(
            lines.contains(&"src/main.rs:1: TODO: add feature".to_string()),
            "got: {out}"
        );
        assert!(
            lines.contains(&"src/lib.rs:1: const TODO: u32 = 1;".to_string()),
            "got: {out}"
        );
        assert!(
            lines.contains(&"notes.md:1: TODO in markdown".to_string()),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn no_matches_returns_marker() {
        let tree = Tree::new("nomatch");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "ZZZ_NOT_HERE",
            None,
            None,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(out, "No matches found");
    }

    #[tokio::test]
    async fn ignore_case_matches() {
        let tree = Tree::new("ic");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "todo",
            None,
            None,
            true,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            out.contains("vendor/dep.rs:1: todo lowercase todo"),
            "got: {out}"
        );
        assert!(
            out.contains("src/main.rs:1: TODO: add feature"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn literal_mode_treats_pattern_as_fixed_string() {
        let tree = Tree::new("lit");
        // Pattern is a regex metachar sequence; literal mode must find the
        // literal text, regex mode would not.
        fs::write(tree.root.join("src").join("regex.txt"), "a.c\n").unwrap();
        let out = grep_execute(
            &tree.root.display().to_string(),
            "a.c",
            None,
            None,
            false,
            true,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("src/regex.txt:1: a.c"), "got: {out}");
    }

    #[tokio::test]
    async fn glob_filters_files() {
        let tree = Tree::new("glob");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "TODO",
            None,
            Some("*.rs"),
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            out.contains("src/main.rs:1: TODO: add feature"),
            "got: {out}"
        );
        assert!(!out.contains("notes.md"), "got: {out}");
    }

    #[tokio::test]
    async fn file_search_uses_basename() {
        let tree = Tree::new("file");
        let file = tree.root.join("src").join("main.rs");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "TODO",
            Some(&file.display().to_string()),
            None,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("main.rs:1: TODO: add feature"), "got: {out}");
    }

    #[tokio::test]
    async fn gitignore_respected_only_inside_git_repo() {
        // Upstream grep does NOT pass rg --no-require-git, so rg honors
        // .gitignore only when a .git dir is present. Assert both halves of
        // that actual contract.
        let tree = Tree::new("gi");
        fs::write(tree.root.join(".gitignore"), "vendor/\n").unwrap();

        let out = grep_execute(
            &tree.root.display().to_string(),
            "todo",
            None,
            None,
            true,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            out.contains("vendor/dep.rs"),
            "expected vendor hit outside git repo: got {out}"
        );

        fs::create_dir_all(tree.root.join(".git")).unwrap();
        let out = grep_execute(
            &tree.root.display().to_string(),
            "todo",
            None,
            None,
            true,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !out.contains("vendor/dep.rs"),
            "expected vendor ignored inside git repo: got {out}"
        );
    }

    #[tokio::test]
    async fn context_prints_neighbor_lines() {
        let tree = Tree::new("ctx");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "TODO",
            None,
            None,
            false,
            false,
            Some(1),
            None,
        )
        .await
        .unwrap();
        let lines: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        assert!(
            lines.contains(&"src/main.rs:1: TODO: add feature".to_string()),
            "got: {out}"
        );
        assert!(
            lines.contains(&"src/main.rs-2- line two".to_string()),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn limit_caps_matches_with_notice() {
        let tree = Tree::new("limit");
        let out = grep_execute(
            &tree.root.display().to_string(),
            "TODO",
            None,
            None,
            false,
            false,
            None,
            Some(1),
        )
        .await
        .unwrap();
        let lines: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect();
        assert_eq!(lines.len(), 2, "got: {out:?}");
        // rg traversal order across matching files is not contractual; with
        // limit=1 the single hit may be any of the three TODO files.
        assert!(
            lines[0] == "src/main.rs:1: TODO: add feature"
                || lines[0] == "src/lib.rs:1: const TODO: u32 = 1;"
                || lines[0] == "notes.md:1: TODO in markdown",
            "unexpected first match: {:?}; got: {out:?}",
            lines[0]
        );
        assert!(
            out.contains("1 matches limit reached. Use limit=2 for more, or refine pattern"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn long_lines_truncated_with_notice() {
        let tree = Tree::new("long");
        let long = "x".repeat(600);
        fs::write(tree.root.join("src").join("long.txt"), format!("{long}\n")).unwrap();
        let out = grep_execute(
            &tree.root.display().to_string(),
            "xxx",
            None,
            None,
            false,
            true,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("... [truncated]"), "got: {out}");
        assert!(
            out.contains("Some lines truncated to 500 chars. Use read tool to see full lines"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let root = std::env::temp_dir().join(format!("pi-grep-missing-{}", uuid::Uuid::new_v4()));
        let err = grep_execute(
            &root.display().to_string(),
            "x",
            Some("nope"),
            None,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("Path not found"), "got: {err}");
        let _ = fs::remove_dir_all(&root);
    }
}
