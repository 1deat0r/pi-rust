#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_coding_agent::core::settings::{SettingsManager, SettingsMap};
use pi_coding_agent::interactive::selectors::settings_selector_items;

#[test]
fn settings_selector_matches_upstream_main_list_order_and_count() {
    let settings = SettingsManager::in_memory(SettingsMap::new());
    let entries = settings_selector_items(&settings);
    let supports_images = pi_tui::terminal_image::get_capabilities().images.is_some();

    let mut expected = vec![
        "autocompact",
        "auto-resize-images",
        "block-images",
        "skill-commands",
        "show-hardware-cursor",
        "editor-padding",
        "output-padding",
        "autocomplete-max-visible",
        "clear-on-shrink",
        "terminal-progress",
        "steering-mode",
        "follow-up-mode",
        "transport",
        "http-idle-timeout",
        "hide-thinking",
        "mermaid-rendering",
        "cache-miss-notices",
        "collapse-changelog",
        "quiet-startup",
        "install-telemetry",
        "default-project-trust",
        "double-escape-action",
        "tree-filter-mode",
        "warnings",
        "model-thinking",
        "tui-mode",
        "fullscreen-exit-output",
        "fullscreen-scrollbar",
        "theme",
    ];
    if supports_images {
        expected.splice(1..1, ["show-images", "image-width-cells"]);
    }

    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(entries.len(), if supports_images { 31 } else { 29 });

    let find = |id: &str| entries.iter().find(|entry| entry.id == id).unwrap();
    assert_eq!(find("autocompact").label, "Auto-compact");
    assert_eq!(
        find("autocompact").description.as_deref(),
        Some("Automatically compact context when it gets too large")
    );
    assert_eq!(
        find("http-idle-timeout").values.as_ref().unwrap(),
        &vec![
            "30 sec".to_string(),
            "1 min".to_string(),
            "2 min".to_string(),
            "5 min".to_string(),
            "disabled".to_string(),
        ]
    );
    assert_eq!(
        find("default-project-trust").values.as_ref().unwrap(),
        &vec![
            "Ask".to_string(),
            "Always trust".to_string(),
            "Never trust".to_string(),
        ]
    );
    assert_eq!(find("warnings").current_value, "configure");
    assert_eq!(find("model-thinking").current_value, "none");
}
