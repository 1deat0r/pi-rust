#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_coding_agent::interactive::selectors::{
    thinking_selector_items, thinking_selector_items_for_model, SelectorAction, ThinkingSelector,
};
use pi_tui::{Component, TuiKey};

#[test]
fn thinking_selector_distinguishes_selection_and_default_actions() {
    let mut selector = ThinkingSelector::new(thinking_selector_items(), "high", Some("medium"));
    assert_eq!(selector.selected_thinking_level().as_deref(), Some("high"));
    let rendered = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(rendered.contains("Moderate reasoning (~8k tokens) · default"));
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(4))
    );

    let mut selector = ThinkingSelector::new(thinking_selector_items(), "high", Some("medium"));
    assert_eq!(
        selector.handle(&TuiKey::ctrl("s")),
        SelectorAction::SelectAsDefault(Some(4))
    );
    assert_eq!(selector.selected_thinking_level().as_deref(), Some("high"));
}

#[test]
fn thinking_selector_preserves_selected_level_through_filter_and_cancels() {
    let mut selector = ThinkingSelector::new(thinking_selector_items(), "high", Some("medium"));
    for character in "reason".chars() {
        assert_eq!(
            selector.handle(&TuiKey::simple(character.to_string())),
            SelectorAction::None
        );
    }
    assert_eq!(selector.selected_thinking_level().as_deref(), Some("high"));

    assert_eq!(
        selector.handle(&TuiKey::simple("escape")),
        SelectorAction::Cancel
    );
}

#[test]
fn thinking_selector_uses_the_current_models_supported_levels() {
    let model = pi_ai::model::Model::new("text-only", "Text-only", "openai-responses", "provider");
    let levels = pi_ai::model::get_supported_thinking_levels(&model);
    let items = thinking_selector_items_for_model(&levels);

    assert_eq!(
        items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        vec!["off"]
    );
    let selector = ThinkingSelector::new(items, "off", Some("off"));
    let rendered = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(rendered.contains("→ off"));
    assert!(!rendered.contains("high"));
}
