#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::components::select_list::{plain_theme, SelectItem, SelectListLayoutOptions};
use pi_tui::components::{
    extract_selection, AltScreenSearchComponent, Editor, EditorOptions, EditorTheme, ScrollView,
    SelectList, SelectionPoint, SelectionRange, Text,
};
use pi_tui::{
    set_keybindings, Component, KeybindingsConfig, KeybindingsManager, Scene, ScrollbarMode,
    SharedComponent, TerminalBackend, TerminalEvent, TuiAltScreen, TuiKey, TuiStopOptions,
    TUI_KEYBINDINGS,
};

fn text(value: &str) -> SharedComponent {
    Arc::new(Mutex::new(Text::new(value, 0, 0, None)))
}

#[test]
fn registry_dispatches_all_matching_actions_without_eviction() {
    let mut config = KeybindingsConfig::new();
    config.insert("tui.input.submit".into(), vec!["ctrl+x".into()]);
    config.insert("tui.select.confirm".into(), vec!["ctrl+x".into()]);
    let manager = KeybindingsManager::new(TUI_KEYBINDINGS, config);

    let key = TuiKey::ctrl("x");
    assert_eq!(
        manager.matching_bindings(&key),
        vec!["tui.input.submit", "tui.select.confirm"]
    );
    let mut dispatched = Vec::new();
    let chosen = manager.dispatch(&key, |action| {
        dispatched.push(action.to_string());
        action == "tui.select.confirm"
    });
    assert_eq!(chosen.as_deref(), Some("tui.select.confirm"));
    assert_eq!(dispatched, vec!["tui.input.submit", "tui.select.confirm"]);
    assert!(manager.matches_raw("\x18", "tui.input.submit"));
}

#[test]
fn editor_dispatches_custom_registry_action() {
    let original = pi_tui::get_keybindings();
    let mut config = KeybindingsConfig::new();
    config.insert("tui.editor.cursorRight".into(), vec!["ctrl+x".into()]);
    config.insert("tui.select.down".into(), vec!["ctrl+x".into()]);
    set_keybindings(KeybindingsManager::new(TUI_KEYBINDINGS, config));

    let mut editor = Editor::new(
        3,
        EditorTheme {
            border_color: Arc::new(|text| text.to_string()),
        },
        EditorOptions::default(),
    );
    editor.set_text("abc");
    editor.handle_input("home");
    editor.handle_input("ctrl+x");
    assert_eq!(editor.get_cursor(), (0, 1));

    let mut list = SelectList::new(
        vec![
            SelectItem::new("one", "one", None),
            SelectItem::new("two", "two", None),
        ],
        2,
        plain_theme(),
        SelectListLayoutOptions::default(),
    );
    list.handle_input(&TuiKey::ctrl("x"));
    assert_eq!(list.selected_index(), 1);

    set_keybindings(original);
}

#[test]
fn selection_snaps_narrow_and_cjk_graphemes() {
    let lines = vec!["A界🙂e\u{301}Z".to_string(), "tail".to_string()];
    let selected = extract_selection(
        &lines,
        SelectionRange {
            start: SelectionPoint { row: 0, column: 2 },
            end: SelectionPoint { row: 0, column: 4 },
        },
    );
    assert_eq!(selected, "界🙂");

    let multiline = extract_selection(
        &lines,
        SelectionRange {
            start: SelectionPoint { row: 0, column: 5 },
            end: SelectionPoint { row: 1, column: 2 },
        },
    );
    assert_eq!(multiline, "e\u{301}Z\ntai");
}

#[test]
fn scene_root_preserves_primary_viewport_and_pi_page_overlap() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(24, 6)));
    let transcript = text(
        &(1..=20)
            .map(|line| format!("transcript {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let scroll = Arc::new(Mutex::new(ScrollView::with_options(
        transcript,
        true,
        pi_tui::ScrollOverscroll::Chain,
    )));
    let footer = text("footer");
    let scene = Arc::new(Mutex::new(Scene::new(
        vec![scroll.clone(), footer],
        Some(0),
    )));

    let mut tui = TuiAltScreen::new(terminal);
    let root: SharedComponent = scene;
    tui.set_layout_root(Some(root));
    tui.render_now(true);
    assert_eq!(scroll.lock().unwrap().viewport_height(), 5);
    assert_eq!(tui.viewport_top(), 15);
    assert!(tui.is_following_output());

    assert!(tui.dispatch_viewport_input("pageup"));
    assert_eq!(tui.viewport_top(), 14);
    assert!(!tui.is_following_output());
    assert!(tui.dispatch_viewport_input("pagedown"));
    assert_eq!(tui.viewport_top(), 15);
    assert!(tui.is_following_output());
}

#[test]
fn automatic_scrollbar_expires_after_inactivity() {
    let child = text(
        &(1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let mut view = ScrollView::with_options(child, true, pi_tui::ScrollOverscroll::Contain);
    view.set_height(3);
    view.set_scrollbar(ScrollbarMode::Auto);
    view.render(20);
    view.set_scrollbar_hide_delay(Duration::from_secs(60));
    // Match Pi's hover lifecycle: entering the scrollbar arms transient
    // visibility, and leaving it starts the inactivity countdown.
    view.set_scrollbar_active(true);
    view.set_scrollbar_active(false);
    assert!(view.is_scrollbar_visible());
    assert!(view.refresh_scrollbar(Instant::now() + Duration::from_secs(61)));
    assert!(!view.is_scrollbar_visible());
}

#[test]
fn alt_screen_routes_custom_scrollback_bindings_and_restores_terminal() {
    let original = pi_tui::get_keybindings();
    let mut config = KeybindingsConfig::new();
    config.insert("tui.altScreen.lineUp".into(), vec!["ctrl+y".into()]);
    let manager = KeybindingsManager::new(TUI_KEYBINDINGS, config);
    set_keybindings(manager);

    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
    let child = Arc::new(Mutex::new(ScrollView::with_options(
        text(
            &(1..=12)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let mut tui = TuiAltScreen::new(terminal.clone());
    let child: SharedComponent = child;
    tui.set_layout_root(Some(child));
    tui.render_now(true);
    assert_eq!(tui.viewport_top(), 8);
    tui.dispatch_raw("\x19");
    assert_eq!(tui.viewport_top(), 7);

    // The controller performs the same raw-mode teardown needed before a
    // foreground suspend, and resume re-enters the alternate screen.
    tui.start().unwrap();
    tui.dispatch_raw("\x1a");
    assert!(tui.is_suspended());
    assert!(!terminal.lock().unwrap().is_raw());
    assert!(!terminal.lock().unwrap().is_alt_screen());
    tui.resume().unwrap();
    assert!(!tui.is_suspended());
    assert!(terminal.lock().unwrap().is_raw());
    assert!(terminal.lock().unwrap().is_alt_screen());
    let redraws_before_resize = tui.full_redraws();
    terminal.lock().unwrap().begin_output_capture();
    tui.dispatch_event(TerminalEvent::Resize(12, 3));
    assert_eq!(tui.full_redraws(), redraws_before_resize);
    assert!(tui.is_render_requested());
    assert!(terminal.lock().unwrap().take_output_capture().is_empty());
    tui.render_now(false);
    assert!(tui.full_redraws() > redraws_before_resize);
    tui.stop(TuiStopOptions::default()).unwrap();
    assert!(!terminal.lock().unwrap().is_raw());
    assert!(!terminal.lock().unwrap().is_alt_screen());
    set_keybindings(original);
}

#[test]
fn search_overlay_accepts_query_navigates_and_cancels() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(40, 5)));
    let child = Arc::new(Mutex::new(ScrollView::with_options(
        text("needle one\nother\nneedle two\nlast"),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let mut tui = TuiAltScreen::new(terminal);
    let child: SharedComponent = child;
    tui.set_layout_root(Some(child));
    tui.start().unwrap();
    tui.dispatch_raw("\x1b[102;6u");
    assert!(tui.has_overlay());
    tui.dispatch_raw("needle");
    tui.dispatch_raw("\x07");
    tui.dispatch_raw("\x1b");
    assert!(!tui.has_overlay());
    tui.stop(TuiStopOptions::default()).unwrap();

    // Keep the component public and constructible for embedders that provide
    // their own search controller.
    let mut search = AltScreenSearchComponent::new();
    search.set_query("needle");
    search.set_result(1, 2);
    assert!(search.render(20).iter().any(|line| line.contains("2/2")));
}

#[test]
fn viewport_search_dispatch_requests_repaint_for_open_query_and_close() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(40, 5)));
    let child: SharedComponent = Arc::new(Mutex::new(ScrollView::with_options(
        text("needle one\nneedle two"),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let mut tui = TuiAltScreen::new(terminal);
    tui.set_layout_root(Some(child));
    tui.render_now(true);
    while tui.take_render_request().is_some() {}

    assert!(tui.dispatch_viewport_input("\x1b[102;6u"));
    assert!(tui.take_render_request().is_some());

    assert!(tui.dispatch_viewport_input("needle"));
    assert!(tui.take_render_request().is_some());

    assert!(tui.dispatch_viewport_input("\x1b"));
    assert!(tui.take_render_request().is_some());
}

#[test]
fn viewport_selection_dispatch_requests_owner_repaint() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 3)));
    let child: SharedComponent = Arc::new(Mutex::new(ScrollView::with_options(
        text("first line\nsecond line"),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let mut tui = TuiAltScreen::new(terminal);
    tui.set_layout_root(Some(child));
    tui.render_now(true);
    while tui.take_render_request().is_some() {}

    assert!(tui.dispatch_viewport_input("\x1b[<0;2;1M"));
    assert!(tui.take_render_request().is_some());
}

#[test]
fn suspend_clears_search_and_completed_selection_before_resume() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 3)));
    let child: SharedComponent = Arc::new(Mutex::new(ScrollView::with_options(
        text("first line\nsecond line"),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let mut tui = TuiAltScreen::new(terminal);
    tui.set_layout_root(Some(child));
    tui.start().unwrap();

    tui.dispatch_raw("\x1b[<0;2;1M");
    tui.dispatch_raw("\x1b[<32;6;1M");
    tui.dispatch_raw("\x1b[<0;6;1m");
    assert!(tui.selection().is_some());

    tui.dispatch_raw("\x1b[102;6u");
    assert!(tui.has_overlay());
    tui.suspend().unwrap();
    assert!(!tui.has_overlay());
    assert!(tui.selection().is_none());

    tui.resume().unwrap();
    tui.stop(TuiStopOptions::default()).unwrap();
}

#[test]
fn search_refreshes_against_new_transcript_before_render_returns() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(40, 5)));
    let transcript = Arc::new(Mutex::new(Text::new("old", 0, 0, None)));
    let transcript_component: SharedComponent = transcript.clone();
    let scroll = Arc::new(Mutex::new(ScrollView::with_options(
        transcript_component,
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let scroll_component: SharedComponent = scroll.clone();
    let mut tui = TuiAltScreen::new(terminal.clone());
    tui.set_layout_root(Some(scroll_component));
    tui.start().unwrap();

    tui.dispatch_raw("\x1b[102;6u");
    tui.dispatch_raw("needle");
    transcript.lock().unwrap().set_text("needle");

    terminal.lock().unwrap().begin_output_capture();
    tui.render_now(false);
    let output = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
    assert!(output.contains("1/1"));
    assert!(!output.contains("No matches"));

    tui.dispatch_raw("\x1b");
    tui.stop(TuiStopOptions::default()).unwrap();
}

#[test]
fn alt_screen_mouse_selection_copies_without_splitting_text() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 3)));
    let child = Arc::new(Mutex::new(ScrollView::with_options(
        text("A界🙂e\u{301}Z\nsecond"),
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let child: SharedComponent = child;
    let mut tui = TuiAltScreen::new(terminal.clone());
    tui.set_layout_root(Some(child));
    tui.render_now(true);
    terminal.lock().unwrap().begin_output_capture();
    tui.dispatch_raw("\x1b[<0;2;1M");
    tui.dispatch_raw("\x1b[<32;5;1M");
    tui.dispatch_raw("\x1b[<3;5;1m");
    assert_eq!(tui.selection().as_deref(), Some("界🙂"));
    tui.render_now(false);
    let output = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
    assert!(output.contains("\x1b]52;c;55WM8J+Zgg==\x07"));
    assert!(output.contains("\x1b[7m界🙂\x1b[27m"));
}

#[test]
fn alt_screen_scrollbar_drag_moves_the_retained_viewport() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
    let mut view = ScrollView::with_options(
        text(
            &(1..=12)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        true,
        pi_tui::ScrollOverscroll::Contain,
    );
    view.set_scrollbar(ScrollbarMode::Always);
    let child: SharedComponent = Arc::new(Mutex::new(view));
    let mut tui = TuiAltScreen::new(terminal);
    tui.set_layout_root(Some(child));
    tui.render_now(true);
    assert_eq!(tui.viewport_top(), 8);

    // The four-row track has a two-row thumb at the bottom. Drag its centre
    // to the top; coordinates in SGR mouse reports are one-based.
    tui.dispatch_raw("\x1b[<0;20;3M");
    tui.dispatch_raw("\x1b[<32;20;1M");
    tui.dispatch_raw("\x1b[<0;20;1m");
    assert_eq!(tui.viewport_top(), 0);
}
