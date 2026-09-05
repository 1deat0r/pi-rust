#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_coding_agent::interactive::selectors::{
    model_selector_items_with_state, ListSelector, ModelSelector, ScopedModelsAction,
    ScopedModelsSelector, SelectorAction,
};
use pi_tui::components::select_list::SelectItem;
use pi_tui::keys::{is_key_release, parse_key};
use pi_tui::{Component, TuiKey};

fn model(id: &str, provider: &str) -> pi_ai::model::Model {
    pi_ai::model::Model::new(id, id, "openai-responses", provider)
}

fn item(value: &str, label: &str, description: &str) -> SelectItem {
    SelectItem::new(value, label, Some(description.to_owned()))
}

#[test]
fn model_selector_distinguishes_session_selection_from_default_selection() {
    let mut selector = ModelSelector::new(
        vec![
            model("fallback", "provider"),
            model("preferred", "provider"),
        ],
        Some("provider/fallback".to_owned()),
        Some("provider/preferred".to_owned()),
    );

    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/fallback")
    );
    let rendered = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(rendered.contains("fallback [provider] ✓"));
    assert!(rendered.contains("preferred [provider] · default"));
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(0))
    );
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/fallback")
    );

    selector.handle(&TuiKey::simple("down"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/preferred")
    );
    assert_eq!(
        selector.handle(&TuiKey::ctrl("s")),
        SelectorAction::SelectAsDefault(Some(1))
    );
}

#[test]
fn model_selector_default_search_prioritizes_default_and_clearing_preserves_selection() {
    let mut selector = ModelSelector::new(
        vec![
            model("current", "provider"),
            model("default-candidate", "provider"),
            model("other", "provider"),
        ],
        Some("provider/current".to_owned()),
        Some("provider/default-candidate".to_owned()),
    );

    for character in "def".chars() {
        selector.handle(&TuiKey::simple(character.to_string()));
    }
    assert_eq!(selector.count(), 1);
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/default-candidate")
    );

    selector.handle(&TuiKey::simple("backspace"));
    selector.handle(&TuiKey::simple("backspace"));
    selector.handle(&TuiKey::simple("backspace"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/default-candidate")
    );
}

#[test]
fn model_selector_pages_by_one_visible_window_wraps_and_ignores_release() {
    let models = (0..12)
        .map(|index| model(&format!("model-{index:02}"), "provider"))
        .collect::<Vec<_>>();
    let mut selector = ModelSelector::new(
        models,
        Some("provider/model-00".to_owned()),
        Some("provider/model-01".to_owned()),
    );

    let release = "\x1b[57419;1:3u";
    assert!(is_key_release(release));
    let before = selector.selected_model_reference();
    if !is_key_release(release) {
        selector.handle(&parse_key(release));
    }
    assert_eq!(selector.selected_model_reference(), before);

    selector.handle(&TuiKey::simple("pagedown"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/model-10")
    );
    selector.handle(&TuiKey::simple("pageup"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/model-00")
    );
    selector.handle(&TuiKey::simple("down"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/model-01")
    );
    selector.handle(&TuiKey::simple("up"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/model-00")
    );
    selector.handle(&TuiKey::simple("up"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/model-11")
    );
}

#[test]
fn model_selector_opens_in_session_scope_and_switches_scope_without_losing_current_model() {
    let models = vec![
        model("current", "provider"),
        model("scoped-first", "provider"),
        model("outside", "provider"),
    ];
    let scoped = vec![
        "provider/scoped-first".to_string(),
        "provider/current".to_string(),
    ];
    let mut selector = ModelSelector::new_with_scoped_models(
        models,
        &scoped,
        Some("provider/current".to_owned()),
        None,
    );

    let initial = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(initial.contains("Scope: all | scoped"));
    assert!(initial.contains("Tab scope (all/scoped)"));
    assert!(initial.contains("scoped-first [provider]"));
    assert!(initial.contains("current [provider] ✓"));
    assert!(!initial.contains("outside [provider]"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/current")
    );

    assert_eq!(
        selector.handle(&TuiKey::simple("tab")),
        SelectorAction::Cycle
    );
    let all = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(all.contains("Scope: all | scoped"));
    assert!(all.contains("outside [provider]"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/current")
    );

    assert_eq!(
        selector.handle(&TuiKey::simple("tab")),
        SelectorAction::Cycle
    );
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/current")
    );
    let scoped_again = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(!scoped_again.contains("outside [provider]"));
}

#[test]
fn model_selector_orders_current_then_default_then_provider_and_model() {
    let selector = ModelSelector::new(
        vec![
            model("zeta", "beta"),
            model("alpha", "beta"),
            model("zeta", "alpha"),
            model("default", "omega"),
            model("current", "zulu"),
        ],
        Some("zulu/current".to_owned()),
        Some("omega/default".to_owned()),
    );

    let rendered = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    let positions = [
        "current [zulu]",
        "default [omega]",
        "zeta [alpha]",
        "alpha [beta]",
        "zeta [beta]",
    ]
    .map(|row| rendered.find(row).expect("model row is rendered"));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn model_selector_empty_state_is_safe_and_escape_cancels() {
    let mut selector = ModelSelector::new(Vec::new(), None, None);
    let rendered = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(rendered.contains("No matching models"));
    assert!(rendered.contains("Use /login to add providers"));
    assert_eq!(selector.selected_model(), None);
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::None
    );
    assert_eq!(selector.handle(&TuiKey::ctrl("s")), SelectorAction::None);
    assert_eq!(
        selector.handle(&TuiKey::simple("escape")),
        SelectorAction::Cancel
    );
}

#[test]
fn model_selector_refresh_failures_retain_cached_rows_and_selection() {
    use pi_ai::models::{ModelsError, ModelsErrorCode, ModelsRefreshResult};
    use std::collections::BTreeMap;

    let cached = vec![model("current", "provider"), model("other", "provider")];
    let mut selector =
        ModelSelector::new(cached.clone(), Some("provider/current".to_owned()), None);

    selector.apply_refresh(
        cached.clone(),
        &ModelsRefreshResult {
            aborted: true,
            errors: BTreeMap::new(),
        },
    );
    let timed_out = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(timed_out.contains("Model refresh timed out; showing cached models."));
    assert!(timed_out.contains("current [provider] ✓"));
    assert!(timed_out.contains("other [provider]"));
    assert_eq!(
        selector.selected_model_reference().as_deref(),
        Some("provider/current")
    );

    let mut errors = BTreeMap::new();
    errors.insert(
        "provider".to_owned(),
        ModelsError::new(ModelsErrorCode::ModelSource, "offline"),
    );
    selector.apply_refresh(
        cached,
        &ModelsRefreshResult {
            aborted: false,
            errors,
        },
    );
    let failed = pi_tui::strip_ansi_codes(&selector.render(100).join("\n"));
    assert!(failed.contains("Could not refresh provider; showing cached models."));
    assert!(failed.contains("current [provider] ✓"));
    assert!(failed.contains("other [provider]"));
}

#[test]
fn list_selector_pages_filtered_rows_and_preserves_original_index() {
    let mut selector = ListSelector::new(
        vec![
            item("zero", "Zero", ""),
            item("one", "One", ""),
            item("two", "Two", ""),
            item("three", "Three", ""),
            item("four", "Four", ""),
        ],
        2,
    );
    selector.handle(&TuiKey::simple("pagedown"));
    assert_eq!(selector.selected_index(), 2);
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(2))
    );
}

#[test]
fn scoped_selector_keeps_unavailable_rows_and_model_builder_filters_provider() {
    let mut scoped = ScopedModelsSelector::new(
        vec![item("openai/gpt", "gpt", "OpenAI")],
        &["stale/provider-model".to_owned()],
    );
    assert!(scoped.render(100).join("\n").contains("unavailable"));
    assert_eq!(
        scoped.handle(&TuiKey::simple("enter")),
        ScopedModelsAction::Toggle {
            model: "stale/provider-model".to_owned(),
            enabled: false,
        }
    );

    let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
    models.set_provider(pi_ai::providers::openai_provider());
    models.set_runtime_api_key("openai", "selector-test-key");
    let items = model_selector_items_with_state(&models, Some("openai"), None, None);
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item.value.starts_with("openai/")));
}
