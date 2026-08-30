#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_tui::components::{editor::plain_editor_theme, Editor, EditorOptions};
use pi_tui::tui::Component;
use pi_tui::visible_width;
use pi_tui::word_navigation::Segment;
use pi_tui::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionResult};
use std::sync::atomic::AtomicBool;

fn editor() -> Editor {
    Editor::new(24, plain_editor_theme(), EditorOptions::default())
}

#[test]
fn vertical_movement_maps_terminal_cells_to_utf8_boundaries() {
    let mut editor = editor();
    editor.set_text("a界b\nac");

    // The cursor starts after the two one-cell ASCII characters on line 2.
    // Visual column 2 falls inside the two-cell `界` on line 1 and must snap
    // to that grapheme's start rather than byte offset 2.
    editor.handle_input("up");
    assert_eq!(editor.get_cursor(), (0, "a".len()));
    let rendered = editor.render(80);
    assert!(rendered.iter().all(|line| visible_width(line) <= 80));

    editor.handle_input("down");
    assert_eq!(editor.get_cursor(), (1, 1));
    let rendered = editor.render(80);
    assert!(rendered.iter().all(|line| visible_width(line) <= 80));
}

#[test]
fn wrapped_unicode_lines_remain_renderable_through_vertical_navigation() {
    let mut editor = editor();
    editor.set_text("a界b界c\nabc");

    // Force the first logical line to wrap and exercise every visual row in
    // both directions. The assertion is intentionally render-based: an
    // interior UTF-8 cursor offset would panic in the renderer.
    for _ in 0..4 {
        editor.handle_input("up");
        let rendered = editor.render(5);
        assert!(rendered.iter().all(|line| visible_width(line) <= 5));
    }
    for _ in 0..4 {
        editor.handle_input("down");
        let rendered = editor.render(5);
        assert!(rendered.iter().all(|line| visible_width(line) <= 5));
    }
}

#[test]
fn editor_cursor_never_splits_combining_or_zwj_graphemes() {
    let mut editor = editor();
    editor.set_text("e\u{301}👩‍💻\n界");

    for _ in 0..3 {
        editor.handle_input("up");
        editor.handle_input("down");
        let (line, cursor) = editor.get_cursor();
        let text = editor.get_lines()[line].clone();
        assert!(text.is_char_boundary(cursor));
        assert!(
            cursor == 0
                || cursor == text.len()
                || pi_tui::grapheme_boundaries(&text)
                    .iter()
                    .any(|(_, end)| *end == cursor)
        );
        let _ = editor.render(12);
    }
}

#[test]
fn word_wrap_chunks_are_always_utf8_boundaries_for_wide_text() {
    let text = "a界b界c";
    let chunks = pi_tui::components::editor::word_wrap_line(text, 2, None);
    assert!(!chunks.is_empty());
    for chunk in chunks {
        assert!(text.is_char_boundary(chunk.start_index));
        assert!(text.is_char_boundary(chunk.end_index));
        assert_eq!(&text[chunk.start_index..chunk.end_index], chunk.text);
        assert!(visible_width(&chunk.text) <= 2);
    }
}

#[test]
fn word_wrap_rejects_stale_or_character_indexed_segments_without_slicing_panic() {
    let text = "a界b界c";
    let stale_segments = [Segment {
        // Byte offset 2 is inside the three-byte `界` code point.
        index: 2,
        segment: "界".to_string(),
        is_word_like: true,
    }];
    let chunks = pi_tui::components::editor::word_wrap_line(text, 2, Some(&stale_segments));
    assert!(!chunks.is_empty());
    for chunk in chunks {
        assert!(text.is_char_boundary(chunk.start_index));
        assert!(text.is_char_boundary(chunk.end_index));
    }
}

struct InvalidCompletionCursor;

impl AutocompleteProvider for InvalidCompletionCursor {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["@".to_string()]
    }

    fn get_suggestions(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
        _force: bool,
        _aborted: &AtomicBool,
    ) -> Option<AutocompleteSuggestions> {
        Some(AutocompleteSuggestions {
            items: vec![AutocompleteItem {
                value: "@界".to_string(),
                label: "界".to_string(),
                description: None,
            }],
            prefix: "@".to_string(),
        })
    }

    fn apply_completion(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
        _item: &AutocompleteItem,
        _prefix: &str,
    ) -> CompletionResult {
        // Simulate a character-oriented provider returning an offset inside
        // the three-byte `界` code point.
        CompletionResult {
            lines: vec!["界".to_string()],
            cursor_line: 0,
            cursor_col: 1,
        }
    }
}

#[test]
fn provider_cursor_offsets_are_normalized_before_rendering() {
    let mut editor = editor();
    editor.set_autocomplete_provider(Box::new(InvalidCompletionCursor));
    editor.handle_input("@");
    editor.flush_autocomplete();
    editor.handle_input("tab");

    assert_eq!(editor.get_text(), "界");
    assert_eq!(editor.get_cursor(), (0, 0));
    let _ = editor.render(12);
}
