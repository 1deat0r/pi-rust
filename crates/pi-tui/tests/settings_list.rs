#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::{Arc, Mutex};

use pi_tui::components::settings_list::{
    plain_settings_theme, SettingItem, SettingsList, SettingsListOptions, SettingsSubmenuChangeFn,
    SettingsSubmenuDoneFn,
};
use pi_tui::{strip_ansi_codes, Component, TuiKey, CURSOR_MARKER};

fn items() -> Vec<SettingItem> {
    vec![
        SettingItem::new("blocked", "Blocked", "n/a", vec!["n/a".into()]).with_disabled(true),
        SettingItem::new("one", "One", "1", vec!["1".into(), "one".into()]),
        SettingItem::new("two", "Two", "2", vec!["2".into(), "two".into()]),
        SettingItem::new("three", "Three", "3", vec!["3".into(), "three".into()]),
    ]
}

#[test]
fn navigation_skips_disabled_rows_and_one_raw_press_is_one_move() {
    let mut list = SettingsList::new(
        items(),
        2,
        plain_settings_theme(),
        SettingsListOptions::default(),
    );

    assert_eq!(list.selected_id().as_deref(), Some("one"));
    list.handle_input(&TuiKey::simple("down"));
    assert_eq!(list.selected_id().as_deref(), Some("two"));
    list.handle_input(&TuiKey::simple("up"));
    assert_eq!(list.selected_id().as_deref(), Some("one"));

    // A Kitty CSI-u release must not be interpreted as a second movement.
    list.handle_raw_input("\x1b[57420;1:3u");
    assert_eq!(list.selected_id().as_deref(), Some("one"));
    list.handle_raw_input("\x1b[57420u");
    assert_eq!(list.selected_id().as_deref(), Some("two"));

    list.set_disabled("two", true);
    assert_eq!(list.selected_id().as_deref(), Some("one"));
    let blocked_row = list
        .render(40)
        .into_iter()
        .find(|line| strip_ansi_codes(line).contains("Blocked"))
        .expect("disabled row remains visible");
    assert!(!strip_ansi_codes(&blocked_row).starts_with("→ "));
}

#[test]
fn page_movement_uses_visible_page_size_and_wraps_enabled_rows() {
    let mut list = SettingsList::new(
        vec![
            SettingItem::new("one", "One", "1", Vec::new()),
            SettingItem::new("two", "Two", "2", Vec::new()),
            SettingItem::new("three", "Three", "3", Vec::new()),
            SettingItem::new("four", "Four", "4", Vec::new()),
        ],
        2,
        plain_settings_theme(),
        SettingsListOptions::default(),
    );

    list.handle_input(&TuiKey::simple("pagedown"));
    assert_eq!(list.selected_id().as_deref(), Some("three"));
    list.handle_input(&TuiKey::simple("pageup"));
    assert_eq!(list.selected_id().as_deref(), Some("one"));
}

struct EscapeDoneChild {
    done: Option<SettingsSubmenuDoneFn>,
    focused: Arc<Mutex<bool>>,
}

impl Component for EscapeDoneChild {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["child submenu".to_string()]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if key.base == "escape" || key.base == "esc" {
            if let Some(done) = self.done.take() {
                done(None, None);
            }
        }
    }

    fn set_focused(&mut self, focused: bool) {
        *self.focused.lock().unwrap() = focused;
    }
}

#[test]
fn nested_escape_closes_child_restores_parent_selection_and_focus() {
    let focused = Arc::new(Mutex::new(false));
    let focused_for_submenu = focused.clone();
    let item = SettingItem::new("picker", "Picker", "old", Vec::new()).with_submenu_done(
        move |_current, done| {
            Some(Box::new(EscapeDoneChild {
                done: Some(done),
                focused: focused_for_submenu.clone(),
            }))
        },
    );
    let cancels = Arc::new(Mutex::new(0usize));
    let cancels_for_callback = cancels.clone();
    let mut list = SettingsList::new_with_callbacks(
        vec![item, SettingItem::new("other", "Other", "", Vec::new())],
        5,
        plain_settings_theme(),
        |_, _| {},
        move || *cancels_for_callback.lock().unwrap() += 1,
        SettingsListOptions {
            enable_search: true,
        },
    );

    list.set_focused(true);
    assert!(list.render(40)[0].contains(CURSOR_MARKER));
    list.handle_input(&TuiKey::simple("enter"));
    assert!(list.is_submenu_open());
    assert!(*focused.lock().unwrap());

    // Escape belongs to the nested component. The parent cancel callback is
    // not invoked, and focus returns to the parent search/input slot.
    list.handle_input(&TuiKey::simple("esc"));
    assert!(!list.is_submenu_open());
    assert_eq!(list.selected_id().as_deref(), Some("picker"));
    assert_eq!(*cancels.lock().unwrap(), 0);
    assert!(list.render(40)[0].contains(CURSOR_MARKER));
    assert!(!*focused.lock().unwrap());
}

struct LiveChild {
    done: Option<SettingsSubmenuDoneFn>,
    on_change: Option<SettingsSubmenuChangeFn>,
}

impl Component for LiveChild {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["live submenu".to_string()]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if key.base == "enter" {
            if let Some(on_change) = &self.on_change {
                on_change("updated".to_string());
            }
        } else if key.base == "escape" || key.base == "esc" {
            if let Some(done) = self.done.take() {
                done(None, None);
            }
        }
    }
}

#[test]
fn live_submenu_changes_reach_parent_without_closing_until_escape() {
    let item = SettingItem::new("live", "Live", "initial", Vec::new()).with_submenu_callbacks(
        |_current, done, on_change| {
            Some(Box::new(LiveChild {
                done: Some(done),
                on_change: Some(on_change),
            }))
        },
    );
    let changes = Arc::new(Mutex::new(Vec::new()));
    let changes_for_callback = changes.clone();
    let mut list = SettingsList::new_with_callbacks(
        vec![item],
        10,
        plain_settings_theme(),
        move |id, value| {
            changes_for_callback
                .lock()
                .unwrap()
                .push((id.to_string(), value.to_string()));
        },
        || {},
        SettingsListOptions::default(),
    );

    list.handle_input(&TuiKey::simple("enter"));
    assert!(list.is_submenu_open());
    list.handle_input(&TuiKey::simple("enter"));
    assert!(list.is_submenu_open());
    assert_eq!(
        changes.lock().unwrap().as_slice(),
        [("live".to_string(), "updated".to_string())]
    );
    list.handle_input(&TuiKey::simple("escape"));
    assert!(!list.is_submenu_open());
}
