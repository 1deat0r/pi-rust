#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_tui::components::{editor::plain_editor_theme, Editor, EditorOptions};
use pi_tui::{
    get_keybindings, set_keybindings, KeybindingsConfig, KeybindingsManager, TUI_KEYBINDINGS,
};

struct RestoreKeybindings(pi_tui::KeybindingsManager);

impl Drop for RestoreKeybindings {
    fn drop(&mut self) {
        set_keybindings(self.0.clone());
    }
}

#[test]
fn dedicated_history_bindings_browse_without_cursor_movement() {
    let original = get_keybindings();
    let _restore = RestoreKeybindings(original);

    let mut config = KeybindingsConfig::new();
    config.insert("tui.editor.historyPrevious".into(), vec!["ctrl+p".into()]);
    config.insert("tui.editor.historyNext".into(), vec!["ctrl+n".into()]);
    set_keybindings(KeybindingsManager::new(TUI_KEYBINDINGS, config));

    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.add_to_history("older prompt");
    editor.add_to_history("newer\nmultiline prompt");
    editor.set_text("draft");
    editor.handle_input("left");
    editor.handle_input("left");

    // Dedicated history navigation enters history directly, even though the
    // draft cursor is not at a line edge.
    editor.handle_input("\x10"); // Ctrl+P
    assert_eq!(editor.get_text(), "newer\nmultiline prompt");
    assert_eq!(editor.get_cursor(), (0, 0));

    editor.handle_input("\x10"); // Ctrl+P
    assert_eq!(editor.get_text(), "older prompt");

    editor.handle_input("\x0e"); // Ctrl+N
    assert_eq!(editor.get_text(), "newer\nmultiline prompt");
    assert_eq!(editor.get_cursor(), (1, 16));

    editor.handle_input("\x0e"); // Ctrl+N
    assert_eq!(editor.get_text(), "draft");
    assert_eq!(editor.get_cursor(), (0, 3));
}

#[test]
fn legacy_shift_enter_sequences_insert_multiline_composer_breaks() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.handle_input("one");
    editor.handle_input("\n");
    editor.handle_input("two");
    assert_eq!(editor.get_text(), "one\ntwo");
    assert_eq!(editor.get_cursor(), (1, 3));

    editor.handle_input("\x1b\r");
    assert_eq!(editor.get_text(), "one\ntwo\n");
    assert_eq!(editor.get_cursor(), (2, 0));

    editor.handle_input("three");
    editor.handle_input("\x1b[13;2~");
    assert_eq!(editor.get_text(), "one\ntwo\nthree\n");
    assert_eq!(editor.get_cursor(), (3, 0));
}

#[test]
fn direct_printable_batch_preserves_single_grapheme_clusters() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.handle_input("👩‍💻");
    assert_eq!(editor.get_text(), "👩‍💻");
    editor.handle_input("backspace");
    assert_eq!(editor.get_text(), "");
}

#[test]
fn bracketed_paste_keeps_text_before_and_after_a_complete_marker() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.handle_input("before\x1b[200~pasted\x1b[201~after");
    assert_eq!(editor.get_text(), "beforepastedafter");
}

#[test]
fn bracketed_paste_end_marker_can_be_split_across_reads() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.handle_input("before\x1b[200~paste\x1b[20");
    assert_eq!(editor.get_text(), "before");
    editor.handle_input("1~after");
    assert_eq!(editor.get_text(), "beforepasteafter");
}

#[test]
fn bracketed_paste_decodes_kitty_ctrl_newline() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.handle_input("\x1b[200~one\x1b[106;5utwo\x1b[201~");
    assert_eq!(editor.get_text(), "one\ntwo");
    assert_eq!(editor.get_cursor(), (1, "two".len()));
}

#[test]
fn editor_word_navigation_groups_cjk_runs_but_keeps_fullwidth_punctuation() {
    let original_kitty_state = pi_tui::keys::is_kitty_protocol_active();
    pi_tui::keys::set_kitty_protocol_active(false);

    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.set_text("你好，世界");
    editor.handle_input("\x1b[1;5D");
    assert_eq!(editor.get_cursor(), (0, "你好，".len()));
    editor.handle_input("\x1b[1;5D");
    assert_eq!(editor.get_cursor(), (0, "你好".len()));
    editor.handle_input("\x1b[1;5D");
    assert_eq!(editor.get_cursor(), (0, 0));

    editor.handle_input("\x1b[1;5C");
    assert_eq!(editor.get_cursor(), (0, "你好".len()));
    editor.handle_input("\x1b[1;5C");
    assert_eq!(editor.get_cursor(), (0, "你好，".len()));
    editor.handle_input("\x1b[1;5C");
    assert_eq!(editor.get_cursor(), (0, "你好，世界".len()));

    editor.set_text("hello你好，world世界");
    for expected in [
        "hello你好，world".len(),
        "hello你好，".len(),
        "hello你好".len(),
        "hello".len(),
        0,
    ] {
        editor.handle_input("\x1b[1;5D");
        assert_eq!(editor.get_cursor(), (0, expected));
    }

    pi_tui::keys::set_kitty_protocol_active(original_kitty_state);
}

#[test]
fn deleting_a_paste_marker_does_not_delete_following_text() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.set_text("prepost");
    editor.handle_input("home");
    for _ in 0..3 {
        editor.handle_input("right");
    }

    let large = (0..15).map(|i| format!("line {i}\n")).collect::<String>();
    editor.handle_input(&format!("\x1b[200~{large}\x1b[201~"));
    assert!(editor.get_text().starts_with("pre[paste #1 +"));
    editor.handle_input("backspace");
    assert_eq!(editor.get_text(), "prepost");
    assert_eq!(editor.get_expanded_text(), "prepost");
}

#[test]
fn multiline_kills_accumulate_and_yank_as_one_entry() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.set_text("line1\nline2\nline3");
    for _ in 0..5 {
        editor.handle_input("ctrl+u");
    }
    assert_eq!(editor.get_text(), "");
    editor.handle_input("ctrl+y");
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
}

#[test]
fn undo_restores_the_state_before_history_browsing() {
    let mut editor = Editor::new(24, plain_editor_theme(), EditorOptions::default());
    editor.add_to_history("saved");
    editor.handle_input("draft");
    editor.handle_input("ctrl+w");
    editor.handle_input("up");
    assert_eq!(editor.get_text(), "saved");
    editor.handle_input("ctrl+-");
    assert_eq!(editor.get_text(), "");
    editor.handle_input("ctrl+-");
    assert_eq!(editor.get_text(), "draft");
}
