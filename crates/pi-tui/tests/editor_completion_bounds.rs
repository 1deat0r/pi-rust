#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use pi_tui::components::{editor::plain_editor_theme, Editor, EditorOptions};
use pi_tui::tui::Component;
use pi_tui::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionResult};
use std::sync::atomic::AtomicBool;

fn editor() -> Editor {
    Editor::new(24, plain_editor_theme(), EditorOptions::default())
}

struct MalformedCompletion {
    empty_lines: bool,
}

impl AutocompleteProvider for MalformedCompletion {
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
                value: "@completion".to_string(),
                label: "completion".to_string(),
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
        CompletionResult {
            lines: if self.empty_lines {
                Vec::new()
            } else {
                vec!["界".to_string()]
            },
            cursor_line: usize::MAX,
            cursor_col: usize::MAX,
        }
    }
}

fn apply_malformed_completion(empty_lines: bool) -> Editor {
    let mut editor = editor();
    editor.set_autocomplete_provider(Box::new(MalformedCompletion { empty_lines }));
    editor.handle_input("@");
    editor.flush_autocomplete();
    editor.handle_input("tab");
    editor
}

#[test]
fn completion_cursor_line_is_clamped_before_rendering() {
    let editor = apply_malformed_completion(false);

    assert_eq!(editor.get_lines(), vec!["界".to_string()]);
    // The invalid line clamps to 0; the invalid column clamps to the valid
    // end of the three-byte grapheme.
    assert_eq!(editor.get_cursor(), (0, 3));
    let _ = editor.render(12);
}

#[test]
fn empty_completion_result_preserves_editor_line_invariant() {
    let editor = apply_malformed_completion(true);

    assert_eq!(editor.get_lines(), vec![String::new()]);
    assert_eq!(editor.get_cursor(), (0, 0));
    let _ = editor.render(12);
}
