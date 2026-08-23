//! `ls` tool — port of `packages/coding-agent/src/core/tools/ls.ts`.
//!
//! Lists directory entries sorted case-insensitively, `'/'`-suffixed for
//! directories, dotfiles included. Output truncated to 500 entries or 50KB
//! (whichever is hit first).

use std::path::Path;
use std::sync::Arc;

use pi_agent::tools::path_utils::resolve_tool_path;
use pi_agent::tools::truncate::{truncate_head_with, DEFAULT_MAX_BYTES};
use pi_agent::tools::{AgentTool, AgentToolResult};
use pi_ai::types::json_tool;

use super::bytes_limit_notice;

const DEFAULT_LIMIT: u64 = 500;

/// Model-facing execute: returns the text output for `ls`.
pub async fn ls_execute(
    cwd: &str,
    path: Option<&str>,
    limit: Option<u64>,
) -> Result<String, String> {
    let dir_path = resolve_tool_path(cwd, path.unwrap_or("."));
    let effective_limit = limit.unwrap_or(DEFAULT_LIMIT);

    let path_obj = Path::new(&dir_path);
    if !path_obj.exists() {
        return Err(format!("Path not found: {dir_path}"));
    }
    let stat =
        std::fs::metadata(&dir_path).map_err(|e| format!("Failed to stat {dir_path}: {e}"))?;
    if !stat.is_dir() {
        return Err(format!("Not a directory: {dir_path}"));
    }

    let mut entries: Vec<String> = std::fs::read_dir(&dir_path)
        .map_err(|e| format!("Cannot read directory: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    // Sort alphabetically, case-insensitive (JS localeCompare on lowercase);
    // stable sort keeps readdir order for equal foldings, like upstream.
    entries.sort_by_key(|e| e.to_lowercase());

    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in &entries {
        if results.len() >= effective_limit as usize {
            entry_limit_reached = true;
            break;
        }
        let full_path = Path::new(&dir_path).join(entry);
        let suffix = match std::fs::metadata(&full_path) {
            Ok(m) if m.is_dir() => "/",
            Ok(_) => "",
            Err(_) => continue,
        };
        results.push(format!("{entry}{suffix}"));
    }

    if results.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    let raw_output = results.join("\n");
    let truncation = truncate_head_with(&raw_output, 1_000_000, DEFAULT_MAX_BYTES);
    let mut output = truncation.content;
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit * 2
        ));
    }
    if truncation.truncated {
        notices.push(bytes_limit_notice());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(output)
}

/// Builds the `ls` tool bound to a working directory.
pub fn ls_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "ls",
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list (default: current directory)"},
                    "limit": {"type": "number", "description": "Maximum number of entries to return (default: 500)"}
                }
            }),
        ),
        "List directory",
        Arc::new(move |_tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args.get("path").and_then(|v| v.as_str());
                let limit = args.get("limit").and_then(|v| v.as_u64());
                match ls_execute(&cwd, path, limit).await {
                    Ok(output) => Ok(AgentToolResult::text(output)),
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
            let root = std::env::temp_dir().join(format!("pi-ls-{tag}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();
            fs::create_dir_all(root.join(".hidden-dir")).unwrap();
            fs::write(root.join("Cargo.toml"), "").unwrap();
            fs::write(root.join("README.md"), "").unwrap();
            fs::write(root.join("src").join("main.rs"), "").unwrap();
            fs::write(root.join("src").join("lib.rs"), "").unwrap();
            fs::write(root.join("vendor").join("dep.rs"), "").unwrap();
            fs::write(root.join(".hidden-dir").join("secret.rs"), "").unwrap();
            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn lists_entries_sorted_with_dir_suffix_and_dotfiles() {
        let tree = Tree::new("basic");
        let out = ls_execute(&tree.root.display().to_string(), None, None)
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // Sorted case-insensitively; '.' folds before letters so dotfiles
        // sort first (upstream localeCompare on lowercase strings).
        assert_eq!(lines[0], ".hidden-dir/");
        assert_eq!(lines[1], "Cargo.toml");
        assert_eq!(lines[2], "README.md");
        assert_eq!(lines[3], "src/");
        assert_eq!(lines[4], "vendor/");
    }

    #[tokio::test]
    async fn empty_directory_returns_marker() {
        let root = std::env::temp_dir().join(format!("pi-ls-empty-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let out = ls_execute(&root.display().to_string(), None, None)
            .await
            .unwrap();
        assert_eq!(out, "(empty directory)");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let root = std::env::temp_dir().join(format!("pi-ls-missing-{}", uuid::Uuid::new_v4()));
        let err = ls_execute(&root.display().to_string(), Some("nope"), None)
            .await
            .unwrap_err();
        assert!(err.contains("Path not found"), "got: {err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn file_path_errors_not_directory() {
        let tree = Tree::new("file");
        let err = ls_execute(&tree.root.display().to_string(), Some("Cargo.toml"), None)
            .await
            .unwrap_err();
        assert!(err.contains("Not a directory"), "got: {err}");
    }

    #[tokio::test]
    async fn limit_caps_entries_and_notices() {
        let tree = Tree::new("limit");
        let out = ls_execute(&tree.root.display().to_string(), None, Some(2))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // 2 entries, a blank line, then the notice line.
        assert_eq!(lines.len(), 4, "got: {out:?}");
        assert_eq!(lines[0], ".hidden-dir/");
        assert_eq!(lines[1], "Cargo.toml");
        assert!(
            out.contains("2 entries limit reached. Use limit=4 for more"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn sort_is_case_insensitive() {
        let root = std::env::temp_dir().join(format!("pi-ls-case-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("BETA.txt"), "").unwrap();
        fs::write(root.join("alpha.txt"), "").unwrap();
        fs::write(root.join("Gamma.txt"), "").unwrap();
        let out = ls_execute(&root.display().to_string(), None, None)
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["alpha.txt", "BETA.txt", "Gamma.txt"]);
        let _ = fs::remove_dir_all(&root);
    }
}
