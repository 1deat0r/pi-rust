#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_agent::session::types::Entry;
use pi_agent::types::AgentMessage;
use pi_ai::types::{Message, UserContent};
use pi_coding_agent::core::settings::{SettingsManager, SettingsMap};
use pi_coding_agent::interactive::config_selector::{
    ConfigSelectorComponent, PathMetadata, ResolvedPaths, ResolvedResource, ResourceOrigin,
    SourceScope,
};
use pi_coding_agent::interactive::selectors::{
    filter_model_items, ListSelector, ModelSelector, ScopedModelsAction, ScopedModelsSelector,
    SelectorAction, ThinkingSelector,
};
use pi_coding_agent::interactive::tree_selector::{TreeSelector, TreeSelectorAction};
use pi_tui::components::select_list::SelectItem;
use pi_tui::keys::{is_key_release, parse_key};
use pi_tui::{Component, TuiKey};

fn tree_user_entry(id: &str, parent_id: Option<&str>, text: &str, seq: u64) -> Entry {
    Entry::Message {
        id: id.to_owned(),
        seq,
        parent_id: parent_id.map(str::to_owned),
        timestamp: seq,
        message: AgentMessage::Core(Message::User(UserContent::string(text, seq))),
        terminate: None,
    }
}

fn item(value: &str, label: &str, description: &str) -> SelectItem {
    SelectItem::new(value, label, Some(description.to_owned()))
}

fn type_text(selector: &mut ListSelector, text: &str) {
    for character in text.chars() {
        assert_eq!(
            selector.handle(&TuiKey::simple(character.to_string())),
            SelectorAction::None
        );
    }
}

fn type_scoped_text(selector: &mut ScopedModelsSelector, text: &str) {
    for character in text.chars() {
        assert_eq!(
            selector.handle(&TuiKey::simple(character.to_string())),
            ScopedModelsAction::None
        );
    }
}

fn visible(lines: &[String]) -> String {
    pi_tui::strip_ansi_codes(&lines.join("\n"))
}

#[test]
fn list_selector_filters_unicode_and_accepts_original_item_index() {
    let mut selector = ListSelector::new(
        vec![
            item("alpha", "Alpha", "first"),
            item("beta", "Beta", "second"),
            item("日本語", "日本語", "third"),
        ],
        8,
    );

    type_text(&mut selector, "et");
    assert_eq!(selector.search_query(), "et");
    assert_eq!(
        selector.selected_item().map(|selected| selected.value),
        Some("beta".into())
    );
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(1)),
        "filtered acceptance must hand the composer the original item index"
    );

    let mut unicode = ListSelector::new(
        vec![
            item("日本語", "日本語", "CJK model"),
            item("other", "Other", "fallback"),
        ],
        8,
    );
    type_text(&mut unicode, "日本");
    assert_eq!(unicode.search_query(), "日本");
    assert_eq!(
        unicode.selected_item().map(|selected| selected.value),
        Some("日本語".into())
    );
    unicode.handle(&TuiKey::simple("left"));
    unicode.handle(&TuiKey::simple("backspace"));
    assert_eq!(unicode.search_query(), "本");
    assert_eq!(
        unicode.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(0))
    );
}

#[test]
fn list_selector_wraps_boundaries_and_empty_enter_is_a_noop() {
    let mut selector = ListSelector::new(
        vec![
            item("one", "One", ""),
            item("two", "Two", ""),
            item("three", "Three", ""),
        ],
        8,
    );

    selector.handle(&TuiKey::simple("up"));
    assert_eq!(selector.selected_index(), 2);
    selector.handle(&TuiKey::simple("down"));
    assert_eq!(selector.selected_index(), 0);

    type_text(&mut selector, "does-not-exist");
    assert!(selector.selected_item().is_none());
    assert!(visible(&selector.render(80)).contains("No matching commands"));
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::None
    );
    assert_eq!(
        selector.handle(&TuiKey::simple("down")),
        SelectorAction::None
    );
    assert_eq!(selector.handle(&TuiKey::ctrl("c")), SelectorAction::Cancel);

    let mut cancelled = ListSelector::new(vec![item("one", "One", "")], 8);
    assert_eq!(
        cancelled.handle(&TuiKey::simple("escape")),
        SelectorAction::Cancel
    );
}

#[test]
fn list_selector_ignores_kitty_release_before_dispatch() {
    let mut selector = ListSelector::new(
        vec![
            item("one", "One", ""),
            item("two", "Two", ""),
            item("three", "Three", ""),
        ],
        8,
    );

    let release = "\x1b[57419;1:3u";
    assert!(is_key_release(release));
    let before_release = selector.selected_index();
    if !is_key_release(release) {
        selector.handle(&parse_key(release));
    }
    assert_eq!(
        selector.selected_index(),
        before_release,
        "a Kitty CSI-u release must not move the selector"
    );

    let press = "\x1b[57419;1u";
    assert!(!is_key_release(press));
    assert_eq!(parse_key(press), TuiKey::simple("up"));
    selector.handle(&parse_key(press));
    assert_eq!(selector.selected_index(), 2);
}

#[test]
fn list_selector_reopen_restores_a_clean_search_surface() {
    for _ in 0..3 {
        let mut selector = ListSelector::new(
            vec![item("alpha", "Alpha", ""), item("beta", "Beta", "")],
            8,
        );
        type_text(&mut selector, "beta");
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            SelectorAction::Select(Some(1))
        );
        assert_eq!(selector.search_query(), "beta");
    }
}

#[test]
fn scoped_selector_preserves_order_handles_unavailable_models_and_reopens() {
    let items = vec![
        item("openai/gpt-5", "gpt-5", "OpenAI"),
        item("anthropic/claude", "claude", "Anthropic"),
        item("google/gemini", "gemini", "Google"),
    ];
    let enabled = vec![
        "anthropic/claude".to_owned(),
        "stale/provider-model".to_owned(),
    ];
    let mut selector = ScopedModelsSelector::new(items.clone(), &enabled);
    assert_eq!(selector.selected_models(), enabled);
    let initial = visible(&selector.render(100));
    assert!(initial.contains("[x] claude"));
    assert!(initial.contains("[x] stale/provider-model"));
    assert!(initial.contains("unavailable"));

    type_scoped_text(&mut selector, "openai");
    assert!(visible(&selector.render(100)).contains("gpt-5"));
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        ScopedModelsAction::Toggle {
            model: "openai/gpt-5".to_owned(),
            enabled: true,
        }
    );
    assert_eq!(selector.search_query(), "openai");
    assert_eq!(
        selector.handle(&TuiKey::ctrl("c")),
        ScopedModelsAction::None
    );
    assert_eq!(selector.search_query(), "");

    // Repeated toggles must not duplicate a canonical provider/model value.
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        ScopedModelsAction::Toggle {
            model: "anthropic/claude".to_owned(),
            enabled: false,
        }
    );
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        ScopedModelsAction::Toggle {
            model: "anthropic/claude".to_owned(),
            enabled: true,
        }
    );
    let selected = selector.selected_models();
    assert_eq!(
        selected
            .iter()
            .filter(|value| *value == "anthropic/claude")
            .count(),
        1
    );

    let reopened = ScopedModelsSelector::new(items, &selected);
    assert_eq!(reopened.selected_models(), selected);
    assert!(visible(&reopened.render(100)).contains("Search:"));

    let mut empty =
        ScopedModelsSelector::new(vec![item("provider/model", "model", "Provider")], &[]);
    type_scoped_text(&mut empty, "missing");
    assert!(visible(&empty.render(100)).contains("No matching models"));
    assert_eq!(
        empty.handle(&TuiKey::simple("enter")),
        ScopedModelsAction::None
    );
    assert_eq!(empty.handle(&TuiKey::ctrl("c")), ScopedModelsAction::None);
    assert_eq!(
        empty.handle(&TuiKey::simple("escape")),
        ScopedModelsAction::Cancel
    );
}

#[test]
fn model_items_expose_id_provider_and_name_search_text() {
    let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
    models.set_provider(pi_ai::providers::openai_provider());
    models.set_runtime_api_key("openai", "unit-test-key");
    let items =
        pi_coding_agent::interactive::selectors::model_selector_items(&models, Some("openai"));
    assert!(!items.is_empty());
    for model in &items {
        let (provider, id) = model
            .value
            .split_once('/')
            .expect("canonical provider/model value");
        assert_eq!(provider, "openai");
        assert_eq!(
            model.label, id,
            "the visible primary label should be the model ID"
        );
        assert!(
            model
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("[openai]"),
            "the provider badge must remain visible"
        );
    }

    let filtered = filter_model_items(
        &[
            item("provider/first", "first", "Friendly Alpha"),
            item("provider/second", "second", "Other"),
        ],
        "friendly",
    );
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].value, "provider/first");
}

#[test]
fn thinking_selector_has_pinned_header_prompt_footer_and_borders() {
    let mut selector = ThinkingSelector::new(
        pi_coding_agent::interactive::selectors::thinking_selector_items(),
        "off",
        Some("off"),
    );
    let rendered = visible(&selector.render(100));
    assert!(rendered.contains("Thinking Level"));
    assert!(rendered.contains("Shift+Tab cycles thinking levels in-session"));
    assert!(rendered.contains("> "));
    assert!(rendered.contains("→ off"));
    assert!(rendered.contains("No reasoning · default"));
    assert!(rendered.contains("Enter to select · Ctrl+S to set as default · Esc to cancel"));
    assert!(!rendered.contains("Search:"));
    assert_eq!(
        rendered.lines().next().map(pi_tui::utils::visible_width),
        Some(100)
    );
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        SelectorAction::Select(Some(0))
    );
}

#[test]
fn model_selector_renders_configured_metadata_and_refresh_state() {
    let current = pi_ai::model::Model::new("gpt-5.5", "GPT-5.5", "openai-codex", "openai-codex");
    let other = pi_ai::model::Model::new(
        "qwen3-coder",
        "Qwen 3 Coder",
        "qwen-token-plan",
        "qwen-token-plan",
    );
    let mut selector = ModelSelector::new(
        vec![current.clone(), other],
        Some("openai-codex/gpt-5.5".to_owned()),
        Some("openai-codex/gpt-5.5".to_owned()),
    );
    let initial = visible(&selector.render(100));
    assert!(initial
        .contains("Only showing models from configured providers. Use /login to add providers."));
    assert!(initial.contains("> "));
    assert!(initial.contains("→ gpt-5.5 [openai-codex] · default ✓"));
    assert!(initial.contains("Model Name: GPT-5.5"));
    assert!(initial.contains("Refreshing model catalogs…"));
    assert!(initial.contains("Enter to select · Ctrl+S to set as default · Esc to cancel"));
    assert!(!initial.contains("Search:"));

    let result = pi_ai::models::ModelsRefreshResult::default();
    selector.apply_refresh(vec![current], &result);
    let refreshed = visible(&selector.render(100));
    assert!(refreshed.contains("Model catalogs refreshed."));
    assert!(refreshed.contains("Model Name: GPT-5.5"));
}

#[test]
fn tree_selector_renders_parent_links_filters_and_cancels_cleanly() {
    let mut selector = TreeSelector::new(
        vec![
            tree_user_entry("root", None, "first prompt", 1),
            tree_user_entry("child", Some("root"), "branch prompt", 2),
        ],
        std::collections::HashMap::new(),
        Some("child".to_owned()),
        30,
    );

    let initial = visible(&selector.render(100));
    assert!(initial.contains("Session Tree"));
    assert!(initial.contains("Type to search:"));
    assert!(initial.contains("└─"));
    assert!(initial.contains("• user: branch prompt"));
    assert!(initial.contains("(2/2)"));

    for character in "branch".chars() {
        assert_eq!(
            selector.handle(&TuiKey::simple(character.to_string())),
            TreeSelectorAction::None
        );
    }
    assert_eq!(selector.count(), 1);
    assert_eq!(selector.selected_entry_id().as_deref(), Some("child"));
    assert_eq!(
        selector.handle(&TuiKey::simple("enter")),
        TreeSelectorAction::Select("child".to_owned())
    );

    // Escape first clears the active search, and only the next Escape closes
    // the modal, matching the selector's two-stage cancellation behavior.
    selector.handle(&TuiKey::simple("escape"));
    assert_eq!(selector.search_query(), "");
    assert_eq!(
        selector.handle(&TuiKey::simple("escape")),
        TreeSelectorAction::Cancel
    );
}

fn resource(path: &str, enabled: bool, scope: SourceScope) -> ResolvedResource {
    ResolvedResource {
        path: path.to_owned(),
        enabled,
        metadata: PathMetadata::synthetic("auto", scope, ResourceOrigin::TopLevel, None),
    }
}

fn settings() -> SettingsManager {
    SettingsManager::in_memory(SettingsMap::new())
}

#[test]
fn config_selector_search_toggle_scope_boundary_unicode_and_empty_state() {
    let global = ResolvedPaths {
        extensions: vec![
            resource("/agent/extensions/alpha.md", true, SourceScope::User),
            resource("/agent/extensions/βeta.md", true, SourceScope::User),
            resource("/agent/extensions/日本語.md", false, SourceScope::User),
        ],
        ..Default::default()
    };
    let project = ResolvedPaths {
        extensions: vec![resource(
            "/project/.pi/extensions/local.md",
            false,
            SourceScope::Project,
        )],
        ..Default::default()
    };
    let mut selector = ConfigSelectorComponent::new(
        global,
        project,
        settings(),
        "/project".to_owned(),
        "/agent".to_owned(),
        "global",
    );

    let initial = visible(&selector.render(120));
    assert!(initial.contains("alpha.md"));
    assert!(initial.contains("βeta.md"));
    assert!(initial.contains("日本語.md"));

    // Config selector navigation stops at both edges instead of wrapping.
    selector.handle_input(&TuiKey::simple("up"));
    assert!(visible(&selector.render(120)).contains(">       [x] alpha.md"));
    selector.handle_input(&TuiKey::simple("down"));
    selector.handle_input(&TuiKey::simple("down"));
    selector.handle_input(&TuiKey::simple("down"));
    assert!(visible(&selector.render(120)).contains(">       [ ] 日本語.md"));

    for character in "β".chars() {
        selector.handle_input(&TuiKey::simple(character.to_string()));
    }
    let filtered = visible(&selector.render(120));
    assert!(filtered.contains("βeta.md"));
    assert!(!filtered.contains("alpha.md"));
    selector.handle_input(&TuiKey::simple(" "));
    assert!(visible(&selector.render(120)).contains(">       [ ] βeta.md"));

    // Switching scope resets to the first item in the new scope.
    selector.handle_input(&TuiKey::simple("backspace"));
    selector.handle_input(&TuiKey::simple("tab"));
    let project_view = visible(&selector.render(120));
    assert!(project_view.contains("Project Local Resources"));
    assert!(project_view.contains(">       [ ] local.md"));

    let mut empty = ConfigSelectorComponent::new(
        ResolvedPaths::default(),
        ResolvedPaths::default(),
        settings(),
        "/project".to_owned(),
        "/agent".to_owned(),
        "global",
    );
    let empty_view = visible(&empty.render(120));
    assert!(empty_view.contains("No resources found"));
    assert!(
        !empty_view.contains("↑/↓ select"),
        "empty upstream view has no footer"
    );
    empty.handle_input(&TuiKey::simple("enter"));
    empty.handle_input(&TuiKey::simple("up"));
    empty.handle_input(&TuiKey::simple("pagedown"));
    empty.handle_input(&TuiKey::simple("escape"));
    assert!(empty.is_closed());
}
