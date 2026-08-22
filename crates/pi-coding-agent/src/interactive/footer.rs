//! Footer component — port of `packages/coding-agent/src/modes/interactive/components/footer.ts`
//! (the readable parts: pwd + git branch + model + thinking + context status).
//!
//! The full upstream footer also shows token usage totals and context
//! percent. This port computes a compact status line from the current model,
//! thinking level, and the number of transcript messages.

use crate::interactive::tui_theme as t;

/// Sanitize text for single-line display.
pub fn sanitize(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace the home directory with `~` in a path.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else { return cwd.to_string() };
    if cwd == home {
        return "~".to_string();
    }
    if let Some(rest) = cwd.strip_prefix(home) {
        if rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

/// Look up the current git branch for a cwd (best-effort, from `git`).
pub fn git_branch(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() { None } else { Some(branch) }
}

/// The footer status payload.
#[derive(Debug, Clone, Default)]
pub struct FooterData {
    pub cwd: String,
    pub branch: Option<String>,
    pub session_name: Option<String>,
    pub model_label: Option<String>,
    pub thinking: Option<String>,
    pub provider_count: usize,
}

/// Render the two footer lines.
pub fn render_footer(data: &FooterData, width: usize) -> Vec<String> {
    let home = std::env::var("HOME").ok();
    let mut pwd = format_cwd_for_footer(&data.cwd, home.as_deref());
    if let Some(branch) = &data.branch {
        pwd = format!("{pwd} ({branch})");
    }
    if let Some(name) = &data.session_name {
        pwd = format!("{pwd} • {name}");
    }

    let mut right = data.model_label.clone().unwrap_or_else(|| "no-model".to_string());
    if let Some(thinking) = &data.thinking {
        if data.thinking.as_deref() != Some("off") {
            right = format!("{right} • {thinking}");
        }
    }
    if data.provider_count > 1 {
        right = format!("({})", right);
    }

    let line1 = truncate_dim(&pwd, width);
    let right_width = pi_tui::utils::visible_width(&right);
    let stats = if right_width <= width {
        let padding = " ".repeat(width.saturating_sub(right_width));
        format!("{right}{padding}")
    } else {
        right
    };
    vec![t::dim(line1), t::dim(stats)]
}

fn truncate_dim(text: &str, width: usize) -> String {
    if pi_tui::utils::visible_width(text) <= width {
        return text.to_string();
    }
    let sliced = pi_tui::utils::slice_with_width(text, width.saturating_sub(3));
    format!("{sliced}...")
}
