//! `ls` tool — port of `packages/coding-agent/src/core/tools/ls.ts`.
//!
//! Lists directory entries sorted case-insensitively, `'/'`-suffixed for
//! directories, dotfiles included. Output truncated to 500 entries or 50KB
//! (whichever is hit first).

use std::path::Path;
use std::sync::{Arc, LazyLock};

use icu_collator::{Collator, CollatorBorrowed};

use pi_agent::tools::path_utils::resolve_tool_path;
use pi_agent::tools::truncate::{truncate_head_with, DEFAULT_MAX_BYTES};
use pi_agent::tools::{AgentTool, AgentToolResult};
use pi_ai::types::json_tool;

use super::{bytes_limit_notice, truncation_details, ToolOutput};

const DEFAULT_LIMIT: u64 = 500;

static ENGLISH_COLLATOR: LazyLock<Option<CollatorBorrowed<'static>>> = LazyLock::new(|| {
    Collator::try_new(icu_locale_core::locale!("en-US").into(), Default::default()).ok()
});

/// Model-facing execute: returns the text output for `ls`.
pub async fn ls_execute(
    cwd: &str,
    path: Option<&str>,
    limit: Option<u64>,
) -> Result<String, String> {
    Ok(
        ls_execute_with_details(cwd, path, limit.map(|value| value as f64), None)
            .await?
            .text,
    )
}

async fn ls_execute_with_details(
    cwd: &str,
    path: Option<&str>,
    limit: Option<f64>,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<ToolOutput, String> {
    if pi_agent::agent::is_aborted(signal.as_ref()) {
        return Err("Operation aborted".to_string());
    }
    let dir_path =
        crate::core::settings::normalize_path(resolve_tool_path(cwd, path.unwrap_or(".")).into())
            .to_string_lossy()
            .into_owned();
    let effective_limit = limit.unwrap_or(DEFAULT_LIMIT as f64);

    let path_obj = Path::new(&dir_path);
    if !path_obj.exists() {
        return Err(format!("Path not found: {dir_path}"));
    }
    let stat =
        std::fs::metadata(&dir_path).map_err(|e| format!("Failed to stat {dir_path}: {e}"))?;
    if !stat.is_dir() {
        return Err(format!("Not a directory: {dir_path}"));
    }

    let read_dir =
        std::fs::read_dir(&dir_path).map_err(|e| format!("Cannot read directory: {e}"))?;
    let mut entries = collect_entry_names(
        read_dir.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned())),
    )?;

    // Match JavaScript's `toLowerCase().localeCompare()` with the default
    // English ICU collation. Stable sorting keeps readdir order for equal
    // lowercased names, like upstream.
    entries.sort_by(|left, right| compare_entry_names(left, right));

    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in &entries {
        if pi_agent::agent::is_aborted(signal.as_ref()) {
            return Err("Operation aborted".to_string());
        }
        if (results.len() as f64) >= effective_limit {
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
        return Ok(ToolOutput {
            text: "(empty directory)".to_string(),
            details: None,
        });
    }

    let raw_output = results.join("\n");
    let truncation = truncate_head_with(&raw_output, 1_000_000, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit * 2.0
        ));
    }
    if truncation.truncated {
        notices.push(bytes_limit_notice());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let mut details = serde_json::Map::new();
    if entry_limit_reached {
        details.insert("entryLimitReached".to_string(), json_limit(effective_limit));
    }
    if truncation.truncated {
        details.insert(
            "truncation".to_string(),
            truncation_details(&truncation, raw_output.len(), 1_000_000),
        );
    }
    Ok(ToolOutput {
        text: output,
        details: (!details.is_empty()).then_some(serde_json::Value::Object(details)),
    })
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
        Arc::new(move |_tool_call_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if pi_agent::agent::is_aborted(signal.as_ref()) {
                    return Err("Operation aborted".to_string());
                }
                let path = args.get("path").and_then(|v| v.as_str());
                let limit = args.get("limit").and_then(|v| v.as_f64());
                match ls_execute_with_details(&cwd, path, limit, signal.clone()).await {
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

fn compare_entry_names(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    ENGLISH_COLLATOR
        .as_ref()
        .map(|collator| collator.compare(&left, &right))
        .unwrap_or_else(|| left.cmp(&right))
}

fn json_limit(limit: f64) -> serde_json::Value {
    if limit >= 0.0 && limit.fract() == 0.0 && limit <= u64::MAX as f64 {
        serde_json::Value::from(limit as u64)
    } else {
        serde_json::json!(limit)
    }
}

fn collect_entry_names<I>(entries: I) -> Result<Vec<String>, String>
where
    I: IntoIterator<Item = Result<String, std::io::Error>>,
{
    entries
        .into_iter()
        .map(|entry| entry.map_err(|error| format!("Cannot read directory: {error}")))
        .collect()
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

    #[test]
    fn directory_iterator_errors_are_not_silently_dropped() {
        let error = collect_entry_names(vec![
            Ok("first".to_string()),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "fixture permission denied",
            )),
        ])
        .unwrap_err();
        assert_eq!(error, "Cannot read directory: fixture permission denied");
    }

    #[tokio::test]
    async fn aborts_before_listing_directory() {
        let tree = Tree::new("aborted");
        let abort = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error =
            ls_execute_with_details(&tree.root.display().to_string(), None, None, Some(abort))
                .await
                .unwrap_err();
        assert_eq!(error, "Operation aborted");
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

    #[tokio::test]
    async fn sorting_matches_upstream_english_locale_compare() {
        let mut names = vec![
            "_under", ".hidden", "-dash", "10", "2", "A", "a", "ä", "á", "å", "aa", "z", "Z", "é",
            "e", "😀",
        ];
        names.sort_by(|left, right| compare_entry_names(left, right));
        assert_eq!(
            names,
            vec![
                "_under", "-dash", ".hidden", "😀", "10", "2", "A", "a", "á", "å", "ä", "aa", "e",
                "é", "z", "Z"
            ]
        );
    }

    #[tokio::test]
    async fn fractional_and_negative_limits_follow_javascript_number_semantics() {
        let tree = Tree::new("numeric-limits");
        let fractional =
            ls_execute_with_details(&tree.root.display().to_string(), None, Some(1.5), None)
                .await
                .unwrap();
        assert_eq!(
            fractional.text,
            ".hidden-dir/\nCargo.toml\n\n[1.5 entries limit reached. Use limit=3 for more]"
        );
        assert_eq!(
            fractional
                .details
                .as_ref()
                .and_then(|details| details.get("entryLimitReached")),
            Some(&serde_json::json!(1.5))
        );

        let negative =
            ls_execute_with_details(&tree.root.display().to_string(), None, Some(-1.0), None)
                .await
                .unwrap();
        assert_eq!(negative.text, "(empty directory)");
        assert!(negative.details.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_entries_follow_targets_and_dangling_links_are_skipped() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("pi-ls-symlinks-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("target-dir")).unwrap();
        fs::write(root.join("target-file"), "fixture").unwrap();
        symlink("target-dir", root.join("dir-link")).unwrap();
        symlink("target-file", root.join("file-link")).unwrap();
        symlink("missing", root.join("dangling-link")).unwrap();

        let out = ls_execute(&root.display().to_string(), None, None)
            .await
            .unwrap();
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec!["dir-link/", "file-link", "target-dir/", "target-file"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_denied_directory_is_actionable_and_secret_safe() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("pi-ls-permission-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let secret = "LS_SECRET_MUST_NOT_LEAK";
        fs::write(root.join("secret.txt"), secret).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let result = ls_execute(&root.display().to_string(), None, None).await;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

        let error = result.expect_err("an unreadable directory must fail");
        assert!(error.contains("Cannot read directory:"), "got: {error}");
        assert!(!error.contains(secret));
        let _ = fs::remove_dir_all(root);
    }
}
