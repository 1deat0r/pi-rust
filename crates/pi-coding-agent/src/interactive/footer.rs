//! Footer component — port of `packages/coding-agent/src/modes/interactive/components/footer.ts`
//! (pwd + git branch + model + thinking + context status + session token totals).

use crate::core::usage_totals::UsageTotals;
use crate::interactive::tui_theme as t;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

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

fn format_tokens_u64(count: u64) -> String {
    if count <= i64::MAX as u64 {
        return format_tokens(count as i64);
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
    let mut sanitized = String::with_capacity(text.len());
    let mut previous_was_ascii_space = false;
    for character in text.chars() {
        let character = match character {
            '\r' | '\n' | '\t' => ' ',
            character => character,
        };
        if character == ' ' {
            if previous_was_ascii_space {
                continue;
            }
            previous_was_ascii_space = true;
        } else {
            previous_was_ascii_space = false;
        }
        sanitized.push(character);
    }
    sanitized.trim().to_string()
}

/// Replace the home directory with `~` in a path.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };

    // Node's `resolve` is lexical: it normalizes `.` and `..`, but does not
    // resolve symlinks. This matters for worktrees and nonexistent paths.
    let resolved_cwd = resolve_footer_path(cwd);
    let resolved_home = resolve_footer_path(home);
    let Some(relative) = relative_footer_path(&resolved_home, &resolved_cwd) else {
        return cwd.to_string();
    };
    if relative.as_os_str().is_empty() {
        return "~".to_string();
    }
    if relative
        .components()
        .next()
        .is_some_and(|component| component == Component::ParentDir)
    {
        return cwd.to_string();
    }
    format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
}

fn resolve_footer_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
            .join(path)
    };
    normalize_footer_path(&absolute)
}

fn normalize_footer_path(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut parts: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else if !has_root {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(value) => parts.push(value.to_os_string()),
        }
    }

    let mut result = PathBuf::new();
    if let Some(prefix) = prefix {
        result.push(prefix);
    }
    if has_root {
        result.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for part in parts {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

fn relative_footer_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from_components: Vec<Component<'_>> = from.components().collect();
    let to_components: Vec<Component<'_>> = to.components().collect();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();

    // On Unix this includes the shared root. On Windows, zero means different
    // volumes, for which Node returns the original cwd.
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &from_components[common..] {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
        relative.push("..");
    }
    for component in &to_components[common..] {
        let Component::Normal(value) = component else {
            return None;
        };
        relative.push(value);
    }
    Some(relative)
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
#[derive(Debug, Clone)]
pub struct FooterData {
    pub cwd: String,
    pub branch: Option<String>,
    pub session_name: Option<String>,
    /// Model id shown on the right side of the footer. `model_label` remains
    /// as a compatibility fallback for embedders using the older payload.
    pub model_id: Option<String>,
    pub model_provider: Option<String>,
    /// Whether the selected provider's configured auth is subscription-backed.
    /// This is resolved from the live provider registry by interactive mode;
    /// it must not be inferred from usage cost because subscription turns have
    /// zero API cost.
    pub using_subscription: bool,
    pub model_label: Option<String>,
    pub thinking: Option<String>,
    pub reasoning: bool,
    pub provider_count: usize,
    /// Current estimated context tokens and model window.
    pub context_tokens: Option<u64>,
    pub context_window: u64,
    pub auto_compact: bool,
    /// Cumulative session usage (upstream `usageTotals`) when available.
    pub usage: Option<UsageTotals>,
    /// Percent of prompt tokens read from cache for the latest assistant turn.
    pub cache_hit_rate: Option<f64>,
}

impl Default for FooterData {
    fn default() -> Self {
        Self {
            cwd: String::new(),
            branch: None,
            session_name: None,
            model_id: None,
            model_provider: None,
            using_subscription: false,
            model_label: None,
            thinking: None,
            reasoning: false,
            provider_count: 0,
            context_tokens: None,
            context_window: 0,
            // Pi's FooterComponent starts with auto-compaction enabled. The
            // interactive mode can override this with the persisted setting.
            auto_compact: true,
            usage: None,
            cache_hit_rate: None,
        }
    }
}

/// Optional footer content supplied by the live extension/runtime provider.
///
/// `FooterData` is also used by embedders and by the retained interactive
/// mode, so adding fields to it would break existing struct literals. Keep
/// provider-owned rows in this separate, additive payload instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FooterExtras {
    /// `(stable status key, single-line status text)` pairs. They are sorted
    /// by key before rendering, matching Pi's extension-status row.
    pub extension_statuses: Vec<(String, String)>,
    /// Whether the experimental feature marker should be shown.
    pub experimental_features: bool,
}

/// Build the token-stats line (upstream `statsParts`): input/output/cacheRead/
/// cacheWrite arrows, cache-hit rate, and total cost.
pub fn render_usage_stats(usage: &UsageTotals, cache_hit_rate: Option<f64>) -> Vec<String> {
    render_usage_stats_with_subscription(usage, cache_hit_rate, false)
}

fn render_usage_stats_with_subscription(
    usage: &UsageTotals,
    cache_hit_rate: Option<f64>,
    using_subscription: bool,
) -> Vec<String> {
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
    if let Some(cache_hit_rate) =
        cache_hit_rate.filter(|_| usage.cache_read > 0 || usage.cache_write > 0)
    {
        parts.push(format!("CH{cache_hit_rate:.1}%"));
    }
    if usage.cost != 0.0 || using_subscription {
        parts.push(if using_subscription {
            format!("${:.3} (sub)", usage.cost)
        } else {
            format!("${:.3}", usage.cost)
        });
    }
    parts
}

/// Render the footer with the process-wide experimental marker enabled when
/// `PI_EXPERIMENTAL=1`. Extension statuses require the explicit extras API
/// because their provider is owned by interactive mode.
pub fn render_footer(data: &FooterData, width: usize) -> Vec<String> {
    render_footer_with_extras(
        data,
        width,
        &FooterExtras {
            experimental_features: crate::core::experimental::are_enabled(),
            ..FooterExtras::default()
        },
    )
}

/// Render footer lines with provider-owned extension statuses and explicit
/// experimental-feature state.
pub fn render_footer_with_extras(
    data: &FooterData,
    width: usize,
    extras: &FooterExtras,
) -> Vec<String> {
    let home = crate::config::home_dir().map(|path| path.to_string_lossy().into_owned());
    let mut pwd = format_cwd_for_footer(&data.cwd, home.as_deref());
    if let Some(branch) = &data.branch {
        pwd = format!("{pwd} ({branch})");
    }
    if let Some(name) = &data.session_name {
        pwd = format!("{pwd} • {name}");
    }

    // Left side: token usage stats if available. Kimi Coding is
    // subscription-backed despite using API-key authentication, exactly as
    // in Pi's footer component.
    let using_subscription =
        data.using_subscription || data.model_provider.as_deref() == Some("kimi-coding");
    let zero_usage = UsageTotals::default();
    let usage = data.usage.as_ref().unwrap_or(&zero_usage);
    let usage_stats =
        render_usage_stats_with_subscription(usage, data.cache_hit_rate, using_subscription)
            .join(" ");

    // Add context usage after the historical token/cost parts. After a
    // compaction the session has no trustworthy estimate until the next
    // assistant turn, and upstream displays that state as `?` rather than
    // manufacturing a zero-percent reading.
    let context_percent = data.context_tokens.map(|context_tokens| {
        if data.context_window > 0 {
            context_tokens as f64 / data.context_window as f64 * 100.0
        } else {
            0.0
        }
    });
    let context_display = match context_percent {
        Some(context_percent) => format!(
            "{context_percent:.1}%/{}{}",
            format_tokens_u64(data.context_window),
            if data.auto_compact { " (auto)" } else { "" }
        ),
        None => format!(
            "?/{}{}",
            format_tokens_u64(data.context_window),
            if data.auto_compact { " (auto)" } else { "" }
        ),
    };
    let context_display = if context_percent.is_some_and(|percent| percent > 90.0) {
        t::fg("error", context_display)
    } else if context_percent.is_some_and(|percent| percent > 70.0) {
        t::fg("warning", context_display)
    } else {
        context_display
    };
    let stats_left = if usage_stats.is_empty() {
        context_display
    } else {
        format!("{usage_stats} {context_display}")
    };
    let stats_left = if extras.experimental_features {
        format!(
            "{stats_left} {} {}",
            t::fg("dim", "•"),
            t::bold(t::fg("warning", "xp"))
        )
    } else {
        stats_left
    };

    // Right side: model id + thinking. The provider is only shown when more
    // than one authenticated provider is available, matching Pi's footer.
    let model_name = data
        .model_id
        .as_deref()
        .or(data.model_label.as_deref())
        .unwrap_or("no-model");
    let mut right_without_provider = model_name.to_string();
    if data.reasoning {
        // Upstream uses `state.thinkingLevel || "off"`; do not omit the
        // indicator when a caller has not supplied a persisted level.
        let thinking = data.thinking.as_deref().unwrap_or("off");
        right_without_provider = if thinking == "off" {
            format!("{model_name} • thinking off")
        } else {
            format!("{model_name} • {thinking}")
        };
    }

    let line1 = pi_tui::truncate_to_width(&t::fg("dim", &pwd), width, &t::fg("dim", "..."));
    let mut stats_left = stats_left;
    let mut stats_left_width = pi_tui::utils::visible_width(&stats_left);
    if stats_left_width > width {
        stats_left = pi_tui::truncate_to_width(&stats_left, width, "...");
        stats_left_width = pi_tui::utils::visible_width(&stats_left);
    }
    let mut right = right_without_provider.clone();
    // The upstream footer only prefixes a provider when a model is selected.
    // A partially populated payload can still carry a provider count; keep
    // the no-model state as `no-model` instead of displaying a misleading
    // provider label beside it.
    if data.provider_count > 1 && (data.model_id.is_some() || data.model_label.is_some()) {
        if let Some(provider) = data.model_provider.as_deref() {
            let candidate = format!("({provider}) {right_without_provider}");
            if stats_left_width + 2 + pi_tui::utils::visible_width(&candidate) <= width {
                right = candidate;
            }
        }
    }
    let right_width = pi_tui::utils::visible_width(&right);
    // Minimum 2 spaces between stats and model.
    let min_padding = 2;
    let stats = if stats_left_width + min_padding + right_width <= width {
        let padding = " ".repeat(
            width
                .saturating_sub(stats_left_width + right_width)
                .max(min_padding),
        );
        format!("{stats_left}{padding}{right}")
    } else {
        // Keep the left status and truncate only the right side when there is
        // room after the required two-cell separator, matching Pi exactly.
        let available_for_right = width.saturating_sub(stats_left_width + min_padding);
        if available_for_right > 0 {
            let truncated_right = pi_tui::truncate_to_width(&right, available_for_right, "");
            let truncated_right_width = pi_tui::utils::visible_width(&truncated_right);
            let padding =
                " ".repeat(width.saturating_sub(stats_left_width + truncated_right_width));
            format!("{stats_left}{padding}{truncated_right}")
        } else {
            stats_left.clone()
        }
    };

    // Pi's footer uses the theme's `dim` foreground token, not SGR 2. Apply
    // it to both portions because context warnings/errors reset foreground.
    let dim_stats_left = t::fg("dim", &stats_left);
    let remainder = stats.strip_prefix(&stats_left).unwrap_or("");
    let dim_remainder = t::fg("dim", remainder);
    let mut lines = vec![line1, format!("{dim_stats_left}{dim_remainder}")];
    if !extras.extension_statuses.is_empty() {
        let mut statuses = extras.extension_statuses.iter().collect::<Vec<_>>();
        statuses.sort_by(|(left, _), (right, _)| left.cmp(right));
        let status_line = statuses
            .into_iter()
            .map(|(_, text)| sanitize(text))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(pi_tui::truncate_to_width(
            &status_line,
            width,
            &t::fg("dim", "..."),
        ));
    }
    lines
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        assert_eq!(format_tokens_u64(u64::MAX), "18446744073710M");
    }

    #[test]
    fn sanitize_only_collapses_the_ascii_whitespace_from_upstream() {
        assert_eq!(
            sanitize("\t  hello \nworld\u{00a0}  pi  \r"),
            "hello world\u{00a0} pi"
        );
        assert_eq!(sanitize("a\u{2003}b"), "a\u{2003}b");
    }

    #[test]
    fn cwd_formatting_matches_resolve_relative_and_sibling_rules() {
        assert_eq!(
            format_cwd_for_footer("/home/user/./project/../src", Some("/home/user")),
            "~/src"
        );
        assert_eq!(
            format_cwd_for_footer("/home/user2/project", Some("/home/user")),
            "/home/user2/project"
        );
        assert_eq!(
            format_cwd_for_footer("/home/user", Some("/home/user/./")),
            "~"
        );
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

    #[test]
    fn render_footer_includes_context_window_and_model_id() {
        let data = FooterData {
            cwd: "/tmp/pi".into(),
            model_id: Some("gpt-5.5".into()),
            model_provider: Some("openai".into()),
            reasoning: true,
            thinking: Some("medium".into()),
            context_tokens: Some(0),
            context_window: 272_000,
            auto_compact: true,
            ..Default::default()
        };
        let lines = render_footer(&data, 100);
        assert!(lines[1].contains("0.0%/272k (auto)"));
        assert!(lines[1].contains("gpt-5.5 • medium"));
        assert!(!lines[1].contains("openai"));
    }

    #[test]
    fn render_footer_uses_off_fallback_and_keeps_narrow_rows_bounded() {
        let data = FooterData {
            model_id: Some("gpt-5.5".into()),
            reasoning: true,
            context_window: 0,
            ..Default::default()
        };
        let line = pi_tui::strip_ansi_codes(&render_footer(&data, 20)[1]);
        assert_eq!(line, "?/0 (auto)  gpt-5.5 ");
        for width in [1, 2, 8, 20, 80, 160] {
            for rendered in render_footer(&data, width) {
                assert!(
                    pi_tui::visible_width(&rendered) <= width,
                    "footer row exceeded width {width}: {rendered:?}"
                );
            }
        }
    }

    #[test]
    fn render_footer_marks_unknown_context_after_compaction() {
        let data = FooterData {
            model_id: Some("gpt-5.5".into()),
            context_tokens: None,
            context_window: 272_000,
            auto_compact: false,
            ..Default::default()
        };
        let line = pi_tui::strip_ansi_codes(&render_footer(&data, 100)[1]);
        assert!(line.contains("?/272k"), "{line}");
        assert!(!line.contains("0.0%/272k"), "{line}");
    }

    #[test]
    fn render_footer_uses_theme_dim_and_context_warning_tokens() {
        t::load_theme(crate::theme::DEFAULT_THEME);
        let data = FooterData {
            cwd: "/tmp/project".into(),
            model_id: Some("gpt-5.5".into()),
            reasoning: true,
            thinking: Some("medium".into()),
            context_tokens: Some(200_000),
            context_window: 200_000,
            auto_compact: false,
            ..Default::default()
        };
        let lines = render_footer(&data, 100);
        let dim_token = t::fg("dim", "x");
        let dim_prefix = dim_token
            .strip_suffix("x\x1b[39m")
            .expect("theme dim token should emit a foreground prefix");
        assert!(lines[0].starts_with(dim_prefix));
        // The pinned dark theme resolves `error -> red -> #cc6666`; the
        // exact escape form depends on the terminal's color capability.
        assert!(lines[1].contains(&t::fg("error", "100.0%/200k")));
        assert!(!lines[1].contains("\x1b[2m"));
    }

    #[test]
    fn render_footer_marks_kimi_cost_as_subscription_even_at_zero() {
        let data = FooterData {
            model_provider: Some("kimi-coding".into()),
            context_window: 200_000,
            ..Default::default()
        };
        let line = pi_tui::strip_ansi_codes(&render_footer(&data, 120)[1]);
        assert!(line.contains("$0.000 (sub)"));
    }

    #[test]
    fn render_footer_marks_live_oauth_subscription_without_fabricating_usage() {
        let data = FooterData {
            model_provider: Some("openai-codex".into()),
            using_subscription: true,
            context_window: 272_000,
            ..Default::default()
        };
        let line = pi_tui::strip_ansi_codes(&render_footer(&data, 120)[1]);
        assert!(line.contains("$0.000 (sub)"));
        assert!(line.contains("?/272k (auto)"));
    }

    #[test]
    fn render_footer_extras_match_experimental_and_extension_status_rows() {
        let data = FooterData {
            model_id: Some("gpt-5.5".into()),
            context_tokens: Some(10_000),
            context_window: 100_000,
            ..Default::default()
        };
        let lines = render_footer_with_extras(
            &data,
            100,
            &FooterExtras {
                extension_statuses: vec![
                    ("zeta".into(), "zeta\tready".into()),
                    ("alpha".into(), " alpha\nready ".into()),
                ],
                experimental_features: true,
            },
        );

        assert_eq!(lines.len(), 3);
        assert!(pi_tui::strip_ansi_codes(&lines[1]).contains("• xp"));
        assert_eq!(
            pi_tui::strip_ansi_codes(&lines[2]),
            "alpha ready zeta ready"
        );
    }

    #[test]
    fn render_footer_extension_status_row_stays_within_terminal_width() {
        let data = FooterData::default();
        let extras = FooterExtras {
            extension_statuses: vec![(
                "extension".into(),
                "status with a very long message that must be clipped".into(),
            )],
            experimental_features: false,
        };
        for width in [1, 2, 8, 20, 40] {
            let lines = render_footer_with_extras(&data, width, &extras);
            assert_eq!(lines.len(), 3);
            assert!(
                pi_tui::visible_width(&lines[2]) <= width,
                "status row exceeded width {width}: {:?}",
                lines[2]
            );
        }
    }

    #[test]
    fn render_footer_does_not_prefix_provider_before_a_model_is_selected() {
        let data = FooterData {
            model_provider: Some("openai".into()),
            provider_count: 2,
            ..Default::default()
        };
        let line = pi_tui::strip_ansi_codes(&render_footer(&data, 80)[1]);
        assert!(line.ends_with("no-model ") || line.ends_with("no-model"));
        assert!(!line.contains("(openai)"));
    }
}
