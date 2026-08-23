//! Footer component — port of `packages/coding-agent/src/modes/interactive/components/footer.ts`
//! (pwd + git branch + model + thinking + context status + session token totals).

use crate::core::usage_totals::UsageTotals;
use crate::interactive::tui_theme as t;

/// Format token counts for compact footer display (upstream `formatTokens`).
pub fn format_tokens(count: i64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round());
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round())
}

/// Sanitize text for single-line display.
pub fn sanitize(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace the home directory with `~` in a path.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };
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
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
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
    /// Cumulative session usage (upstream `usageTotals`) when available.
    pub usage: Option<UsageTotals>,
    /// Percent of prompt tokens read from cache for the latest assistant turn.
    pub cache_hit_rate: Option<f64>,
}

/// Build the token-stats line (upstream `statsParts`): input/output/cacheRead/
/// cacheWrite arrows, cache-hit rate, and total cost.
pub fn render_usage_stats(usage: &UsageTotals, cache_hit_rate: Option<f64>) -> Vec<String> {
    let mut parts = Vec::new();
    if usage.input != 0 {
        parts.push(format!("↑{}", format_tokens(usage.input)));
    }
    if usage.output != 0 {
        parts.push(format!("↓{}", format_tokens(usage.output)));
    }
    if usage.cache_read != 0 {
        parts.push(format!("R{}", format_tokens(usage.cache_read)));
    }
    if usage.cache_write != 0 {
        parts.push(format!("W{}", format_tokens(usage.cache_write)));
    }
    if (usage.cache_read > 0 || usage.cache_write > 0) && cache_hit_rate.is_some() {
        parts.push(format!("CH{:.1}%", cache_hit_rate.unwrap()));
    }
    if usage.cost != 0.0 {
        parts.push(format!("${:.3}", usage.cost));
    }
    parts
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

    // Left side: token usage stats if available.
    let usage_stats = data
        .usage
        .as_ref()
        .map(|u| render_usage_stats(u, data.cache_hit_rate))
        .unwrap_or_default()
        .join(" ");

    // Right side: model + thinking.
    let mut right = data
        .model_label
        .clone()
        .unwrap_or_else(|| "no-model".to_string());
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
    let stats_left_width = pi_tui::utils::visible_width(&usage_stats);
    // Minimum 2 spaces between stats and model.
    let min_padding = 2;
    let stats = if !usage_stats.is_empty() && stats_left_width + min_padding + right_width <= width
    {
        let padding = " ".repeat(
            width
                .saturating_sub(stats_left_width + right_width)
                .max(min_padding),
        );
        format!("{usage_stats}{padding}{right}")
    } else if !usage_stats.is_empty() && stats_left_width <= width {
        // Not enough room for the model; show only stats.
        let padding = " ".repeat(width.saturating_sub(stats_left_width));
        format!("{usage_stats}{padding}")
    } else if right_width <= width {
        let padding = " ".repeat(width.saturating_sub(right_width));
        format!("{right}{padding}")
    } else {
        right
    };
    vec![t::dim(line1), t::dim(stats)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        cost: f64,
    ) -> UsageTotals {
        UsageTotals {
            input,
            output,
            cache_read,
            cache_write,
            cost,
        }
    }

    #[test]
    fn format_tokens_matches_upstream_thresholds() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(2500), "2.5k");
        assert_eq!(format_tokens(9999), "10.0k");
        assert_eq!(format_tokens(10000), "10k");
        assert_eq!(format_tokens(123456), "123k");
        assert_eq!(format_tokens(999999), "1000k");
        assert_eq!(format_tokens(1000000), "1.0M");
        assert_eq!(format_tokens(2500000), "2.5M");
        assert_eq!(format_tokens(10000000), "10M");
        assert_eq!(format_tokens(20000000), "20M");
    }

    #[test]
    fn usage_stats_renders_nonzero_only() {
        let stats = render_usage_stats(&totals(1000, 500, 0, 0, 0.001), Some(30.0));
        // cacheRead is 0 so CH is not shown even though rate present.
        assert_eq!(stats, vec!["↑1.0k", "↓500", "$0.001"]);
    }

    #[test]
    fn usage_stats_includes_cache_read_write_and_hit_rate() {
        let stats = render_usage_stats(&totals(10, 20, 300, 400, 0.0), Some(80.0));
        assert!(stats.contains(&"R300".to_string()));
        assert!(stats.contains(&"W400".to_string()));
        assert!(stats.contains(&"CH80.0%".to_string()));
        assert!(
            !stats.iter().any(|s| s.starts_with('$')),
            "zero cost omitted"
        );
    }

    #[test]
    fn usage_stats_empty_for_zero_usage() {
        assert!(render_usage_stats(&totals(0, 0, 0, 0, 0.0), None).is_empty());
    }

    #[test]
    fn render_footer_places_usage_left_and_model_right() {
        let mut data = FooterData {
            cwd: "/tmp/pi".into(),
            model_label: Some("gpt/model".into()),
            ..Default::default()
        };
        data.usage = Some(totals(1000, 500, 0, 0, 0.001));
        let lines = render_footer(&data, 80);
        let stats_line = lines[1].to_string();
        assert!(stats_line.contains("↑1.0k"));
        assert!(stats_line.contains("$0.001"));
        assert!(stats_line.contains("gpt/model"));
    }
}

fn truncate_dim(text: &str, width: usize) -> String {
    if pi_tui::utils::visible_width(text) <= width {
        return text.to_string();
    }
    let sliced = pi_tui::utils::slice_with_width(text, width.saturating_sub(3));
    format!("{sliced}...")
}
