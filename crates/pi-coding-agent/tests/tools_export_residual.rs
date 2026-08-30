#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused residual coverage for the coding-agent tool/export boundaries.
//!
//! These tests intentionally exercise the real `rg` process and the public
//! `AgentTool` callback, rather than asserting only on helper strings.

use std::fs;
use std::path::PathBuf;

use pi_ai::types::ContentBlock;
use pi_coding_agent::core::tools::{grep_tool, ls_tool};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("pi-tools-export-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn grep_tool_preserves_structured_match_limit_details() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let dir = TempDir::new("grep-details");
    fs::write(dir.0.join("notes.txt"), "needle one\nneedle two\n").unwrap();
    let tool = grep_tool(dir.0.display().to_string());
    let result = (tool.execute)(
        "call-1".to_string(),
        json!({"pattern": "needle", "limit": 1}),
        None,
        None,
    )
    .await
    .unwrap();

    assert!(matches!(
        result.content.first(),
        Some(ContentBlock::Text { .. })
    ));
    assert_eq!(
        result
            .details
            .as_ref()
            .and_then(|details| details.get("matchLimitReached"))
            .and_then(|value| value.as_u64()),
        Some(1)
    );
}

#[tokio::test]
async fn ls_tool_reports_pre_cancel_without_touching_filesystem() {
    let dir = TempDir::new("ls-cancel");
    fs::write(dir.0.join("sentinel.txt"), "sentinel").unwrap();
    let tool = ls_tool(dir.0.display().to_string());
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let result = (tool.execute)("call-2".to_string(), json!({}), Some(cancelled), None).await;

    assert_eq!(result.unwrap_err(), "Operation aborted");
    assert!(dir.0.join("sentinel.txt").exists());
}
