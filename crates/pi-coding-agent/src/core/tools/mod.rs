//! Built-in coding-agent tools — port of
//! `packages/coding-agent/src/core/tools/` (`ls`, `find`, `grep`). The
//! model-facing text output is 1:1; interactive TUI rendering (theme,
//! `renderCall`/`renderResult`) is deferred until pi-tui lands.

pub mod find;
pub mod grep;
pub mod ls;

pub use find::find_tool;
pub use grep::grep_tool;
pub use ls::ls_tool;

use pi_agent::tools::truncate::DEFAULT_MAX_BYTES;

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
