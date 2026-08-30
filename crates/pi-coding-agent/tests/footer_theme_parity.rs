#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused black-box parity tests for the footer and interactive theme lane.
//!
//! This is an integration target so it can run while unrelated `#[cfg(test)]`
//! fixtures in the actively changing interactive mode are incomplete.

use pi_coding_agent::core::usage_totals::UsageTotals;
use pi_coding_agent::interactive::footer::{self, FooterData};
use pi_coding_agent::interactive::tui_theme;

fn totals(input: i64, output: i64, cache_read: i64, cache_write: i64, cost: f64) -> UsageTotals {
    UsageTotals {
        input,
        output,
        cache_read,
        cache_write,
        cost,
    }
}

fn force_truecolor_for_test() {
    pi_tui::terminal_image::set_capabilities(pi_tui::terminal_image::TerminalCapabilities {
        images: None,
        true_color: true,
        hyperlinks: false,
    });
}

#[test]
fn footer_matches_pi_strings_and_separator_at_normal_width() {
    force_truecolor_for_test();
    tui_theme::load_theme("dark");
    let data = FooterData {
        cwd: "/tmp/pi".into(),
        branch: Some("main".into()),
        model_id: Some("gpt-5.5".into()),
        model_provider: Some("openai".into()),
        reasoning: true,
        thinking: Some("medium".into()),
        provider_count: 2,
        context_tokens: Some(27_200),
        context_window: 272_000,
        auto_compact: true,
        usage: Some(totals(1_000, 500, 300, 400, 0.001)),
        cache_hit_rate: Some(75.0),
        ..Default::default()
    };

    let lines = footer::render_footer(&data, 100);
    assert_eq!(pi_tui::strip_ansi_codes(&lines[0]), "/tmp/pi (main)");
    let stats = pi_tui::strip_ansi_codes(&lines[1]);
    assert!(stats.starts_with("↑1.0k ↓500 R300 W400 CH75.0% $0.001 10.0%/272k (auto)"));
    assert!(stats.ends_with("(openai) gpt-5.5 • medium"));
    assert_eq!(pi_tui::visible_width(&lines[1]), 100);
}

#[test]
fn footer_matches_pi_truncation_at_narrow_and_wide_widths() {
    let data = FooterData {
        model_id: Some("gpt-5.5".into()),
        reasoning: true,
        context_window: 0,
        ..Default::default()
    };
    let narrow = pi_tui::strip_ansi_codes(&footer::render_footer(&data, 20)[1]);
    assert_eq!(narrow, "?/0 (auto)  gpt-5.5 ");

    for width in [1, 2, 8, 20, 40, 80, 160] {
        for line in footer::render_footer(&data, width) {
            assert!(
                pi_tui::visible_width(&line) <= width,
                "row exceeds width {width}: {line:?}"
            );
        }
    }
}

#[test]
fn footer_matches_pi_path_sanitization_context_colors_and_subscription() {
    force_truecolor_for_test();
    tui_theme::load_theme("dark");
    assert_eq!(
        footer::format_cwd_for_footer("/home/user/./project/../src", Some("/home/user")),
        "~/src"
    );
    assert_eq!(
        footer::format_cwd_for_footer("/home/user2/project", Some("/home/user")),
        "/home/user2/project"
    );
    assert_eq!(
        footer::sanitize("\t  hello \nworld\u{00a0}  pi  \r"),
        "hello world\u{00a0} pi"
    );

    let data = FooterData {
        model_provider: Some("kimi-coding".into()),
        context_tokens: Some(200_000),
        context_window: 200_000,
        auto_compact: false,
        ..Default::default()
    };
    let line = footer::render_footer(&data, 120);
    // The pinned dark theme resolves `error -> red -> #cc6666`.
    assert!(line[1].contains("\x1b[38;2;204;102;102m100.0%/200k"));
    assert!(pi_tui::strip_ansi_codes(&line[1]).contains("$0.000 (sub)"));
}

#[test]
fn theme_tokens_match_pinned_dark_theme_and_thinking_border_mapping() {
    force_truecolor_for_test();
    tui_theme::load_theme("dark");
    assert_eq!(
        tui_theme::fg("borderMuted", "─"),
        "\x1b[38;2;80;80;80m─\x1b[39m"
    );
    assert_eq!(
        tui_theme::bg("selectedBg", "x"),
        "\x1b[48;2;58;58;74mx\x1b[49m"
    );
    assert_eq!(
        tui_theme::thinking_border("medium")("─"),
        "\x1b[38;2;129;162;190m─\x1b[39m"
    );
    assert_eq!(
        tui_theme::thinking_border("off")("─"),
        "\x1b[38;2;80;80;80m─\x1b[39m"
    );
    assert_eq!(
        tui_theme::bash_mode_border()("─"),
        "\x1b[38;2;181;189;104m─\x1b[39m"
    );
}
