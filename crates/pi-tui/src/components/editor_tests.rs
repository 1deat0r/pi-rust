#[cfg(test)]
mod editor_tests {
    use crate::components::editor::{plain_editor_theme, Editor, EditorOptions};
    use crate::autocomplete::{CombinedAutocompleteProvider, SlashCommand};

    fn editor(rows: usize) -> Editor {
        Editor::new(rows, plain_editor_theme(), EditorOptions::default())
    }

    #[test]
    fn history_up_with_empty_history_does_nothing() {
        let mut e = editor(24);
        e.handle_input("up");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn history_up_shows_most_recent_when_empty() {
        let mut e = editor(24);
        e.add_to_history("first prompt");
        e.add_to_history("second prompt");
        e.handle_input("up");
        assert_eq!(e.get_text(), "second prompt");
    }

    #[test]
    fn history_up_cycles_through_entries() {
        let mut e = editor(24);
        e.add_to_history("first");
        e.add_to_history("second");
        e.add_to_history("third");
        e.handle_input("up");
        assert_eq!(e.get_text(), "third");
        e.handle_input("up");
        assert_eq!(e.get_text(), "second");
        e.handle_input("up");
        assert_eq!(e.get_text(), "first");
        e.handle_input("up");
        assert_eq!(e.get_text(), "first");
    }

    #[test]
    fn history_up_jumps_to_start_before_browsing() {
        let mut e = editor(24);
        e.add_to_history("prompt");
        e.set_text("draft");
        e.handle_input("left");
        e.handle_input("left");
        e.handle_input("up");
        assert_eq!(e.get_text(), "draft");
        assert_eq!(e.get_cursor(), (0, 0));
        e.handle_input("up");
        assert_eq!(e.get_text(), "prompt");
        e.handle_input("down");
        assert_eq!(e.get_text(), "draft");
        assert_eq!(e.get_cursor(), (0, 0));
    }

    #[test]
    fn history_down_navigates_forward() {
        let mut e = editor(24);
        e.add_to_history("first");
        e.add_to_history("second");
        e.add_to_history("third");
        e.set_text("draft");
        e.handle_input("up"); // start of draft
        e.handle_input("up"); // third
        e.handle_input("up"); // second
        e.handle_input("up"); // first
        e.handle_input("down");
        assert_eq!(e.get_text(), "second");
        e.handle_input("down");
        assert_eq!(e.get_text(), "third");
        e.handle_input("down");
        assert_eq!(e.get_text(), "draft");
    }

    #[test]
    fn history_exits_when_typing() {
        let mut e = editor(24);
        e.add_to_history("old prompt");
        e.handle_input("up");
        assert_eq!(e.get_text(), "old prompt");
        e.handle_input("x");
        assert_eq!(e.get_text(), "xold prompt");
    }

    #[test]
    fn history_exits_on_set_text() {
        let mut e = editor(24);
        e.add_to_history("first");
        e.add_to_history("second");
        e.handle_input("up");
        assert_eq!(e.get_text(), "second");
        e.set_text("");
        e.handle_input("up");
        assert_eq!(e.get_text(), "second");
    }

    #[test]
    fn history_skips_empty_and_duplicates() {
        let mut e = editor(24);
        e.add_to_history("");
        e.add_to_history("   ");
        e.add_to_history("valid");
        e.handle_input("up");
        assert_eq!(e.get_text(), "valid");
        e.handle_input("up");
        assert_eq!(e.get_text(), "valid");

        let mut e2 = editor(24);
        e2.add_to_history("same");
        e2.add_to_history("same");
        e2.add_to_history("same");
        e2.handle_input("up");
        assert_eq!(e2.get_text(), "same");
        e2.handle_input("up");
        assert_eq!(e2.get_text(), "same");

        let mut e3 = editor(24);
        e3.add_to_history("first");
        e3.add_to_history("second");
        e3.add_to_history("first");
        e3.handle_input("up");
        assert_eq!(e3.get_text(), "first");
        e3.handle_input("up");
        assert_eq!(e3.get_text(), "second");
        e3.handle_input("up");
        assert_eq!(e3.get_text(), "first");
    }

    #[test]
    fn history_limits_to_100_entries() {
        let mut e = editor(24);
        for i in 0..105 {
            e.add_to_history(&format!("prompt {i}"));
        }
        for _ in 0..100 {
            e.handle_input("up");
        }
        assert_eq!(e.get_text(), "prompt 5");
        e.handle_input("up");
        assert_eq!(e.get_text(), "prompt 5");
    }

    #[test]
    fn up_uses_cursor_movement_when_editor_has_content() {
        let mut e = editor(24);
        e.add_to_history("history item");
        e.set_text("line1\nline2");
        e.handle_input("up");
        e.handle_input("X");
        assert_eq!(e.get_text(), "line1X\nline2");
    }

    #[test]
    fn multiline_browsing_places_cursor_at_start() {
        let mut e = editor(24);
        e.add_to_history("older entry");
        e.add_to_history("line1\nline2\nline3");
        e.handle_input("up");
        assert_eq!(e.get_text(), "line1\nline2\nline3");
        assert_eq!(e.get_cursor(), (0, 0));
    }

    // ---------------- basic editing ----------------

    #[test]
    fn typing_and_backspace() {
        let mut e = editor(24);
        for c in ["h", "e", "l", "l", "o"] {
            e.handle_input(c);
        }
        assert_eq!(e.get_text(), "hello");
        e.handle_input("backspace");
        assert_eq!(e.get_text(), "hell");
    }

    #[test]
    fn cursor_movement_within_line() {
        let mut e = editor(24);
        e.set_text("hello");
        e.handle_input("left");
        e.handle_input("left");
        e.handle_input("X");
        // "helXlo"
        assert_eq!(e.get_text(), "helXlo");
        e.handle_input("home");
        e.handle_input("Y");
        assert_eq!(e.get_text(), "YhelXlo");
        e.handle_input("end");
        e.handle_input("Z");
        assert_eq!(e.get_text(), "YhelXloZ");
    }

    #[test]
    fn delete_forwards() {
        let mut e = editor(24);
        e.set_text("hello");
        e.handle_input("home");
        e.handle_input("delete");
        assert_eq!(e.get_text(), "ello");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_to_line_edges() {
        let mut e = editor(24);
        e.set_text("hello");
        e.handle_input("ctrl+a");
        e.handle_input("X");
        assert_eq!(e.get_text(), "Xhello");
        e.handle_input("ctrl+e");
        e.handle_input("Y");
        assert_eq!(e.get_text(), "XhelloY");
    }

    // ---------------- multi-line ----------------

    #[test]
    fn newline_splits_lines() {
        let mut e = editor(24);
        e.handle_input("a");
        e.handle_input("b");
        e.handle_input("shift+enter");
        e.handle_input("c");
        assert_eq!(e.get_text(), "ab\nc");
        assert_eq!(e.get_cursor(), (1, 1));
    }

    #[test]
    fn up_down_move_between_lines() {
        let mut e = editor(24);
        e.set_text("a\nbb");
        e.handle_input("up");
        e.handle_input("X");
        assert_eq!(e.get_text(), "aX\nbb");
        e.handle_input("down");
        e.handle_input("Y");
        assert_eq!(e.get_text(), "aX\nbbY");
    }

    #[test]
    fn backspace_at_line_start_merges_lines() {
        let mut e = editor(24);
        e.set_text("a\nb");
        // Home moves to line start of "b"; backspace joins the lines.
        e.handle_input("home");
        e.handle_input("backspace");
        assert_eq!(e.get_text(), "ab");
        assert_eq!(e.get_cursor(), (0, 1));
    }

    // ---------------- kill/yank ----------------

    #[test]
    fn kill_to_line_start_and_end() {
        let mut e = editor(24);
        e.set_text("hello world");
        e.handle_input("home");
        e.handle_input("right");
        e.handle_input("right");
        e.handle_input("right");
        e.handle_input("right");
        e.handle_input("right");
        // cursor after "hello"
        e.handle_input("ctrl+k");
        assert_eq!(e.get_text(), "hello");
        e.handle_input("ctrl+u");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn yank_and_yank_pop() {
        let mut e = editor(24);
        e.set_text("first second");
        e.handle_input("ctrl+a");
        e.handle_input("ctrl+k"); // kill "first second" (forward)
        assert_eq!(e.get_text(), "");

        e.handle_input("ctrl+y");
        assert_eq!(e.get_text(), "first second");

        // Kill another entry then yank-pop.
        e.handle_input("ctrl+a");
        e.handle_input("ctrl+w"); // delete word backward at col0 = no-op
        e.handle_input("right");
    }

    #[test]
    fn delete_word_backward() {
        let mut e = editor(24);
        e.set_text("hello world");
        e.handle_input("ctrl+w");
        assert_eq!(e.get_text(), "hello ");
        // Cursor after "hello " -> deletes "hello "
        e.handle_input("ctrl+w");
        e.handle_input("ctrl+w");
        assert_eq!(e.get_text(), "");
    }

    // ---------------- undo ----------------

    #[test]
    fn undo_coalesces_word_characters_into_one_unit() {
        let mut e = editor(24);
        for c in ["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d"] {
            e.handle_input(c);
        }
        assert_eq!(e.get_text(), "hello world");
        // The space captured state before itself, so undo restores "hello".
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "hello");
        // The next snapshot is before the first word char.
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn undo_removes_spaces_one_at_a_time() {
        let mut e = editor(24);
        for c in ["h", "e", "l", "l", "o", " ", " "] {
            e.handle_input(c);
        }
        assert_eq!(e.get_text(), "hello  ");
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "hello ");
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "hello");
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn undo_reverts_settext() {
        let mut e = editor(24);
        e.set_text("draft");
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "");
    }

    // ---------------- submit ----------------

    #[test]
    fn submit_clears_and_returns_text() {
        let mut e = editor(24);
        e.handle_input("h");
        e.handle_input("i");
        e.handle_input("enter");
        assert_eq!(e.drain_submitted(), Some("hi".to_string()));
        assert_eq!(e.get_text(), "");
    }

    // ---------------- paste markers ----------------

    #[test]
    fn large_paste_creates_marker() {
        let mut e = editor(24);
        let big: String = (0..15).map(|i| format!("line {i}\n")).collect();
        e.handle_input("\x1b[200~");
        e.handle_input(&big);
        e.handle_input("\x1b[201~");
        assert!(e.get_text().starts_with("[paste #1 +"));
        assert!(e.get_expanded_text().starts_with("line 0"));
    }

    #[test]
    fn backspace_removes_paste_marker_and_shifts_ids() {
        let mut e = editor(24);
        let big1: String = (0..15).map(|i| format!("one {i}\n")).collect();
        e.handle_input("\x1b[200~");
        e.handle_input(&big1);
        e.handle_input("\x1b[201~");
        let big2: String = (0..15).map(|i| format!("two {i}\n")).collect();
        e.handle_input("\x1b[200~");
        e.handle_input(&big2);
        e.handle_input("\x1b[201~");
        assert!(e.get_text().contains("[paste #2"));
        // Backspace deletes marker #2.
        e.handle_input("backspace");
        assert!(!e.get_text().contains("[paste #2"));
        assert!(e.get_text().contains("[paste #1"));
    }

    // ---------------- autocomplete ----------------

    #[test]
    fn slash_command_autocomplete_and_apply() {
        let mut e = editor(24);
        let commands = vec![
            SlashCommand::new("settings", Some("Open settings".into()), None),
            SlashCommand::new("model", Some("Select model".into()), Some("<provider/model>".into())),
            SlashCommand::new("thinking", Some("Set thinking level".into()), Some("<level>".into())),
        ];
        let provider = Box::new(CombinedAutocompleteProvider::new(commands, "/tmp".to_string(), None));
        e.set_autocomplete_provider(provider);
        e.handle_input("/");
        e.handle_input("m");
        assert!(e.is_showing_autocomplete());
        // Select remains on the fuzzy best match ("model").
        assert_eq!(e.current_autocomplete_selection().map(|i| i.value), Some("model".to_string()));
        e.handle_input("enter");
        assert_eq!(e.drain_submitted(), Some("/model".to_string()));
    }

    #[test]
    fn tab_applies_unique_autocomplete() {
        let mut e = editor(24);
        let commands = vec![
            SlashCommand::new("settings", Some("Open settings".into()), None),
        ];
        let provider = Box::new(CombinedAutocompleteProvider::new(commands, "/tmp".to_string(), None));
        e.set_autocomplete_provider(provider);
        e.handle_input("/set");
        e.handle_input("tab");
        assert!(!e.is_showing_autocomplete());
        assert_eq!(e.get_text(), "/settings ");
    }
    #[test]
    fn slash_autocomplete_enter_submits_slash_command() {
        use crate::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
        let mut e = editor(24);
        let provider = CombinedAutocompleteProvider::new(
            vec![SlashCommand::new("share", Some("Share session as a secret GitHub gist".to_string()), Some(String::new()))],
            "/tmp".to_string(),
            None,
        );
        e.set_autocomplete_provider(Box::new(provider));
        for ch in ["/", "s", "h", "a", "r", "e"] {
            e.handle_input(ch);
            e.update_autocomplete();
        }
        assert!(e.is_showing_autocomplete(), "autocomplete should be open");
        assert!(e.current_autocomplete_selection().is_some(), "slash item should be selectable");
        e.handle_input("enter");
        // Mirror the loop: a few ticks before drain (like the frame loop).
        e.update_autocomplete();
        e.update_autocomplete();
        let submitted = e.drain_submitted();
        assert!(submitted.is_some(), "slash command should be submitted: {:?}", submitted);
        let submitted = submitted.unwrap();
        assert!(submitted.starts_with("/share"), "submitted: {submitted:?}");
    }

    #[test]
    fn slash_autocomplete_enter_without_item_still_submits() {
        let mut e = editor(24);
        e.set_text("/nonsense");
        e.handle_input("enter");
        let submitted = e.drain_submitted();
        assert_eq!(submitted.as_deref(), Some("/nonsense"));
    }
}
