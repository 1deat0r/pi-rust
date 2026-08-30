#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_coding_agent::interactive::settings_panel::{
    SettingChoice, SettingChoiceSubmenu, SettingEntry, SettingsPanel,
};
use pi_tui::{strip_ansi_codes, Component, TuiKey};

#[test]
fn settings_panel_matches_upstream_border_and_description_slots() {
    let panel = SettingsPanel::new(vec![SettingEntry::cycle(
        "mode",
        "TUI mode",
        "regular".to_string(),
        vec!["regular".to_string(), "fullscreen".to_string()],
    )
    .describe("Select the regular or fullscreen interface layout")]);
    let lines = panel.render(60);

    assert_eq!(strip_ansi_codes(&lines[0]), "─".repeat(60));
    assert_eq!(
        strip_ansi_codes(lines.last().expect("bottom border")),
        "─".repeat(60)
    );
    assert!(lines.iter().any(|line| line.contains("> ")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Select the regular or fullscreen interface layout")),
        "rendered lines: {lines:?}"
    );
}

#[test]
fn settings_panel_persists_component_callbacks_for_enter_and_space() {
    let mut panel = SettingsPanel::new(vec![SettingEntry::cycle(
        "mode",
        "TUI mode",
        "regular".to_string(),
        vec!["regular".to_string(), "fullscreen".to_string()],
    )]);

    panel.handle_input(&TuiKey::simple("space"));
    assert_eq!(
        panel.drain_changes(),
        vec![("mode".to_string(), "fullscreen".to_string())]
    );

    panel.handle_input(&TuiKey::simple("enter"));
    assert_eq!(
        panel.drain_changes(),
        vec![("mode".to_string(), "regular".to_string())]
    );
    assert!(panel.drain_changes().is_empty());
}

#[test]
fn settings_panel_projects_disabled_entries_without_blocking_enabled_rows() {
    let mut panel = SettingsPanel::new(vec![
        SettingEntry::info("unavailable", "Unavailable", "managed".to_string()).with_disabled(true),
        SettingEntry::cycle(
            "mode",
            "TUI mode",
            "regular".to_string(),
            vec!["regular".to_string(), "fullscreen".to_string()],
        ),
    ]);

    let lines = panel.render(60);
    let unavailable = lines
        .iter()
        .find(|line| strip_ansi_codes(line).contains("Unavailable"))
        .expect("disabled row remains visible");
    assert!(!strip_ansi_codes(unavailable).starts_with("→ "));

    panel.handle_input(&TuiKey::simple("space"));
    assert_eq!(
        panel.drain_changes(),
        vec![("mode".to_string(), "fullscreen".to_string())]
    );
}

fn theme_entry() -> SettingEntry {
    SettingEntry::choice_submenu(
        "theme",
        "Theme",
        "dark".to_string(),
        "Choose the interface color theme",
        vec![
            SettingChoice::new("dark", "Dark").describe("Dark terminal palette"),
            SettingChoice::new("light", "Light").describe("Light terminal palette"),
            SettingChoice::new("high-contrast", "High contrast"),
        ],
    )
}

#[test]
fn settings_panel_choice_submenu_renders_and_persists_selected_value() {
    let mut panel = SettingsPanel::new(vec![theme_entry()]);
    panel.set_focused(true);

    panel.handle_input(&TuiKey::simple("enter"));
    assert!(panel.is_submenu_open());
    let open_lines = panel.render(70);
    assert!(open_lines.iter().any(|line| line.contains("Theme")));
    assert!(open_lines
        .iter()
        .any(|line| line.contains("Type to filter · Enter to select · Esc to go back")));

    // One physical Down press advances exactly one choice.
    panel.handle_input(&TuiKey::simple("down"));
    let moved_lines = panel.render(70);
    let selected_light = moved_lines
        .iter()
        .find(|line| line.contains("Light"))
        .expect("second choice rendered");
    assert!(strip_ansi_codes(selected_light).starts_with("→ Light"));

    panel.handle_input(&TuiKey::simple("enter"));
    assert!(!panel.is_submenu_open());
    assert_eq!(
        panel.drain_changes(),
        vec![("theme".to_string(), "light".to_string())]
    );
}

#[test]
fn settings_panel_choice_submenu_escape_returns_to_parent_without_change() {
    let mut panel = SettingsPanel::new(vec![theme_entry()]);
    panel.set_focused(true);
    panel.handle_input(&TuiKey::simple("enter"));
    assert!(panel.is_submenu_open());

    panel.handle_input(&TuiKey::simple("esc"));
    assert!(!panel.is_submenu_open());
    assert!(panel.render(70).iter().any(|line| line.contains("Theme")));
    assert!(panel.drain_changes().is_empty());
}

#[test]
fn settings_panel_choice_submenu_pages_and_wraps_one_row_per_arrow() {
    let choices = (0..12)
        .map(|index| SettingChoice::new(format!("value-{index}"), format!("Value {index}")))
        .collect();
    let mut panel = SettingsPanel::new(vec![SettingEntry::choice_submenu(
        "mode",
        "Mode",
        "value-0".to_string(),
        "Choose a mode",
        choices,
    )]);

    panel.handle_input(&TuiKey::simple("enter"));
    panel.handle_input(&TuiKey::simple("pagedown"));
    let page = panel.render(80);
    assert!(page.iter().any(|line| line.contains("(11/12)")));
    panel.handle_input(&TuiKey::simple("down"));
    panel.handle_input(&TuiKey::simple("down"));
    let moved = panel.render(80);
    assert!(moved
        .iter()
        .any(|line| strip_ansi_codes(line).starts_with("→ Value 0")));
}

#[test]
fn settings_panel_keeps_preview_events_out_of_persistence_changes() {
    let mut panel = SettingsPanel::new(vec![SettingEntry::info(
        "theme",
        "Theme",
        "dark".to_string(),
    )
    .with_submenu_preview(|current, done, preview| {
        preview(format!("preview-{current}"));
        Some(Box::new(SettingChoiceSubmenu::new(
            "Theme",
            "Choose a theme",
            vec![SettingChoice::new("dark", "Dark")],
            current,
            done,
        )))
    })]);

    panel.handle_input(&TuiKey::simple("enter"));
    assert_eq!(
        panel.drain_previews(),
        vec![("theme".to_string(), "preview-dark".to_string())]
    );
    assert!(panel.drain_changes().is_empty());

    panel.handle_input(&TuiKey::simple("enter"));
    assert_eq!(
        panel.drain_changes(),
        vec![("theme".to_string(), "dark".to_string())]
    );
    assert!(panel.drain_previews().is_empty());
}

#[test]
fn settings_panel_keeps_callback_submenu_summary_on_the_parent_row() {
    let mut panel = SettingsPanel::new(vec![SettingEntry::info(
        "warnings",
        "Warnings",
        "configure".to_string(),
    )
    .with_submenu_callbacks(|_current, done, on_change| {
        on_change("anthropic-extra-usage=false".to_string());
        Some(Box::new(SettingChoiceSubmenu::new(
            "Warnings",
            "Choose a warning",
            vec![SettingChoice::new("false", "Disabled")],
            "false",
            done,
        )))
    })]);

    panel.handle_input(&TuiKey::simple("enter"));
    assert_eq!(
        panel.drain_changes(),
        vec![(
            "warnings".to_string(),
            "anthropic-extra-usage=false".to_string()
        )]
    );
    panel.handle_input(&TuiKey::simple("escape"));
    assert!(panel.drain_changes().is_empty());
    let parent = strip_ansi_codes(&panel.render(80).join("\n"));
    assert!(parent.contains("Warnings"));
    assert!(parent.contains("configure"));
    assert!(!parent.contains("anthropic-extra-usage=false"));
}

#[test]
fn settings_panel_updates_nested_summary_without_emitting_a_second_change() {
    let mut panel = SettingsPanel::new(vec![SettingEntry::info(
        "model-thinking",
        "Default thinking level per model",
        "none".to_string(),
    )
    .with_submenu_callbacks(|_current, done, _on_change| {
        Some(Box::new(SettingChoiceSubmenu::new(
            "Per-Model Thinking Level",
            "Select a model to configure",
            vec![SettingChoice::new("model", "model [provider]")],
            "model",
            done,
        )))
    })]);

    panel.handle_input(&TuiKey::simple("enter"));
    panel.update_submenu_display_value("model-thinking", "1 configured");
    let nested = strip_ansi_codes(&panel.render(80).join("\n"));
    assert!(nested.contains("Select a model to configure"));

    // Navigation and cancellation must not restore the initial "none"
    // summary or turn the display-only refresh into a persisted change.
    panel.handle_input(&TuiKey::simple("escape"));
    let parent = strip_ansi_codes(&panel.render(80).join("\n"));
    assert!(parent.contains("1 configured"));
    assert!(panel.drain_changes().is_empty());
}
