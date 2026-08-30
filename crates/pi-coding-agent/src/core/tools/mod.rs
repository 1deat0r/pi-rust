//! Built-in coding-agent tools — port of
//! `packages/coding-agent/src/core/tools/` (`ls`, `find`, `grep`). The
//! model-facing text output and interactive TUI rendering are implemented by
//! the Rust tool wrappers and pi-tui components.

pub mod find;
pub mod grep;
pub mod ls;

pub use find::find_tool;
pub use grep::grep_tool;
pub use ls::ls_tool;

use pi_agent::tools::truncate::DEFAULT_MAX_BYTES;
use pi_agent::tools::truncate::{TruncatedBy, Truncation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Internal result used by built-in filesystem tools. The text remains the
/// model-facing contract; details carry the structured state used by the
/// interactive renderer, matching upstream `AgentToolResult.details`.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub text: String,
    pub details: Option<serde_json::Value>,
}

pub(crate) fn truncation_details(
    truncation: &Truncation,
    total_bytes: usize,
    max_lines: usize,
) -> serde_json::Value {
    serde_json::json!({
        "truncated": truncation.truncated,
        "truncatedBy": match truncation.truncated_by {
            Some(TruncatedBy::Lines) => serde_json::Value::String("lines".to_string()),
            Some(TruncatedBy::Bytes) => serde_json::Value::String("bytes".to_string()),
            None => serde_json::Value::Null,
        },
        "totalLines": truncation.total_lines,
        "totalBytes": total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": false,
        "maxLines": max_lines,
        "maxBytes": DEFAULT_MAX_BYTES,
        "firstLineExceedsLimit": truncation.output_lines == 0
            && matches!(truncation.truncated_by, Some(TruncatedBy::Bytes)),
    })
}

/// Upstream `formatSize` (truncate.ts): one decimal for KB/MB.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Byte-truncation notice shared by find/grep/ls (50KB default).
pub fn bytes_limit_notice() -> String {
    format!("{} limit reached", format_size(DEFAULT_MAX_BYTES as u64))
}

/// Wait for a caller-owned abort flag. The upstream tools receive an
/// `AbortSignal`; the Rust tool boundary uses an `AtomicBool`, so polling is
/// the cancellation primitive available here. Process-backed tools use this
/// to terminate their child before returning an abort error.
pub(crate) async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}
