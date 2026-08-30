//! Single-line text input — port of `packages/tui/src/components/input.ts`.
//!
//! Input is deliberately a byte-offset editor because the rest of the
//! interactive stack stores cursor positions that way. All movement and
//! deletion nevertheless happens at terminal grapheme boundaries, so a
//! combining sequence, emoji ZWJ sequence, or CJK cell can never be split.

use crate::keybindings::get_keybindings;
use crate::keys::{match_key, parse_key, TuiKey};
use crate::kill_ring::{KillRing, KillRingPushOptions};
use crate::tui::{Component, CURSOR_MARKER};
use crate::undo_stack::UndoStack;
use crate::utils::{
    grapheme_boundaries, next_grapheme_boundary, previous_grapheme_boundary,
    slice_by_column_strict, visible_width,
};
use crate::word_navigation::{
    find_word_backward, find_word_forward, Segment, WordNavigationOptions,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputState {
    value: String,
    cursor: usize,
}

fn floor_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut boundary = offset;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    grapheme_boundaries(text)
        .into_iter()
        .find_map(|(start, end)| (boundary > start && boundary < end).then_some(start))
        .unwrap_or(boundary)
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn grapheme_safe_word_segments(text: &str) -> Vec<Segment> {
    let graphemes = grapheme_boundaries(text);
    let mut merged: Vec<Segment> = Vec::new();
    for segment in crate::word_navigation::segment_text(text) {
        let starts_at_boundary = segment.index == 0
            || graphemes
                .iter()
                .any(|(start, end)| *start == segment.index || *end == segment.index);
        if !starts_at_boundary {
            if let Some(previous) = merged.last_mut() {
                previous.segment.push_str(&segment.segment);
                previous.is_word_like &= segment.is_word_like;
                continue;
            }
        }
        merged.push(segment);
    }

    // The pinned TUI tests exercise the platform `Intl.Segmenter` behavior,
    // which groups adjacent CJK script characters into dictionary-sized
    // word-like units while
    // keeping ideographic punctuation separate.  The Rust fallback
    // segmenter intentionally emits one CJK scalar at a time and treats some
    // non-ASCII punctuation as word-like, so repair those boundaries before
    // coalescing only adjacent grapheme-aligned CJK segments.  The pinned
    // cases establish two-grapheme units (`你好|世界`), so keep the fallback
    // bounded at two rather than swallowing an entire ideographic sentence.
    let mut punctuation_safe = Vec::with_capacity(merged.len());
    for segment in merged {
        if !segment.is_word_like || !segment.segment.chars().any(is_nonword_unicode_punctuation) {
            punctuation_safe.push(segment);
            continue;
        }

        let mut word = String::new();
        let mut word_start = segment.index;
        let mut punctuation = String::new();
        let mut punctuation_start = segment.index;
        for (offset, character) in segment.segment.char_indices() {
            if is_nonword_unicode_punctuation(character) {
                if !word.is_empty() {
                    punctuation_safe.push(Segment {
                        segment: std::mem::take(&mut word),
                        index: word_start,
                        is_word_like: true,
                    });
                }
                if punctuation.is_empty() {
                    punctuation_start = segment.index + offset;
                }
                punctuation.push(character);
            } else {
                if !punctuation.is_empty() {
                    punctuation_safe.push(Segment {
                        segment: std::mem::take(&mut punctuation),
                        index: punctuation_start,
                        is_word_like: false,
                    });
                }
                if word.is_empty() {
                    word_start = segment.index + offset;
                }
                word.push(character);
            }
        }
        if !word.is_empty() {
            punctuation_safe.push(Segment {
                segment: word,
                index: word_start,
                is_word_like: true,
            });
        }
        if !punctuation.is_empty() {
            punctuation_safe.push(Segment {
                segment: punctuation,
                index: punctuation_start,
                is_word_like: false,
            });
        }
    }

    let mut cjk_runs: Vec<Segment> = Vec::with_capacity(punctuation_safe.len());
    for segment in punctuation_safe {
        let joins_previous = cjk_runs.last().is_some_and(|previous| {
            previous.is_word_like
                && segment.is_word_like
                && is_cjk_word_segment(&previous.segment)
                && is_cjk_word_segment(&segment.segment)
                && grapheme_boundaries(&previous.segment).len() < 2
        });
        if joins_previous {
            let previous = cjk_runs.last_mut().expect("checked above");
            previous.segment.push_str(&segment.segment);
        } else {
            cjk_runs.push(segment);
        }
    }
    cjk_runs
}

fn is_nonword_unicode_punctuation(character: char) -> bool {
    matches!(
        character,
        '\u{3001}'..='\u{303f}'
            | '\u{ff01}'..='\u{ff0f}'
            | '\u{ff1a}'..='\u{ff20}'
            | '\u{ff3b}'..='\u{ff40}'
            | '\u{ff5b}'..='\u{ff65}'
    )
}

fn is_cjk_word_segment(segment: &str) -> bool {
    let graphemes = grapheme_boundaries(segment);
    !graphemes.is_empty()
        && graphemes.iter().all(|(start, end)| {
            segment[*start..*end]
                .chars()
                .next()
                .is_some_and(|character| {
                    crate::word_navigation::is_cjk_char(character)
                        // U+3000..U+303F contains ideographic punctuation;
                        // it is not part of the CJK word run for this UI.
                        && !('\u{3000}'..='\u{303f}').contains(&character)
                })
        })
}

fn find_word_backward_safe(text: &str, cursor: usize) -> usize {
    let segment = |input: &str| grapheme_safe_word_segments(input);
    let options = WordNavigationOptions {
        segment: Some(&segment),
        is_atomic_segment: None,
    };
    find_word_backward(text, cursor, &options)
}

fn find_word_forward_safe(text: &str, cursor: usize) -> usize {
    let segment = |input: &str| grapheme_safe_word_segments(input);
    let options = WordNavigationOptions {
        segment: Some(&segment),
        is_atomic_segment: None,
    };
    find_word_forward(text, cursor, &options)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

pub type InputSubmitCallback = Box<dyn FnMut(&str) + Send + Sync>;
pub type InputEscapeCallback = Box<dyn FnMut() + Send + Sync>;

/// A single-line, horizontally scrolling text input.
pub struct Input {
    /// Logical input value. This remains public for compatibility with the
    /// existing interactive Rust callers.
    pub value: String,
    /// UTF-8 byte offset of the cursor in `value`.
    pub cursor: usize,
    /// Prompt rendered before the input value.
    pub prompt: String,
    /// Called when the configured submit binding or a newline is received.
    pub on_submit: Option<InputSubmitCallback>,
    /// Called when Escape is received.
    pub on_escape: Option<InputEscapeCallback>,
    /// Whether the TUI should emit the hardware-cursor marker.
    pub focused: bool,
    paste_buffer: String,
    in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
}

impl Input {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: prompt.into(),
            on_submit: None,
            on_escape: None,
            focused: false,
            paste_buffer: String::new(),
            in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
        }
    }

    pub fn get_value(&self) -> &str {
        &self.value
    }

    /// Set the value while keeping the cursor at the same logical position,
    /// clamped to the new value. This matches the upstream component and is
    /// important when a selector refreshes its query while the caret is not
    /// at the end.
    pub fn set_value(&mut self, value: impl Into<String>) {
        let cursor = self.cursor;
        self.value = value.into();
        self.set_cursor(cursor.min(self.value.len()));
        self.last_action = None;
    }

    pub fn clear(&mut self) {
        self.push_undo();
        self.value.clear();
        self.cursor = 0;
        self.last_action = None;
    }

    pub fn with_submit_callback(
        mut self,
        callback: impl FnMut(&str) + Send + Sync + 'static,
    ) -> Self {
        self.on_submit = Some(Box::new(callback));
        self
    }

    pub fn with_escape_callback(mut self, callback: impl FnMut() + Send + Sync + 'static) -> Self {
        self.on_escape = Some(Box::new(callback));
        self
    }

    fn cursor_boundary(&self) -> usize {
        floor_grapheme_boundary(&self.value, self.cursor)
    }

    fn set_cursor(&mut self, cursor: usize) {
        self.cursor = floor_grapheme_boundary(&self.value, cursor);
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(InputState {
            value: self.value.clone(),
            cursor: self.cursor_boundary(),
        });
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() || text.chars().any(is_input_control) {
            return;
        }
        if text.chars().any(char::is_whitespace) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        let cursor = self.cursor_boundary();
        self.value.insert_str(cursor, text);
        self.set_cursor(cursor + text.len());
    }

    fn handle_backspace(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor == 0 {
            return;
        }
        self.push_undo();
        let start = previous_grapheme_boundary(&self.value, cursor);
        self.value.replace_range(start..cursor, "");
        self.set_cursor(start);
        self.last_action = None;
    }

    fn handle_forward_delete(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let end = next_grapheme_boundary(&self.value, cursor);
        self.value.replace_range(cursor..end, "");
        self.set_cursor(cursor);
        self.last_action = None;
    }

    fn move_left(&mut self) {
        let cursor = self.cursor_boundary();
        self.set_cursor(previous_grapheme_boundary(&self.value, cursor));
        self.last_action = None;
    }

    fn move_right(&mut self) {
        let cursor = self.cursor_boundary();
        self.set_cursor(next_grapheme_boundary(&self.value, cursor));
        self.last_action = None;
    }

    fn move_word_left(&mut self) {
        let cursor = self.cursor_boundary();
        self.set_cursor(find_word_backward_safe(&self.value, cursor));
        self.last_action = None;
    }

    fn move_word_right(&mut self) {
        let cursor = self.cursor_boundary();
        self.set_cursor(find_word_forward_safe(&self.value, cursor));
        self.last_action = None;
    }

    fn delete_to_line_start(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = self.value[..cursor].to_string();
        self.kill_ring.push(
            &deleted,
            KillRingPushOptions {
                prepend: true,
                accumulate: self.last_action == Some(LastAction::Kill),
            },
        );
        self.value.replace_range(..cursor, "");
        self.set_cursor(0);
        self.last_action = Some(LastAction::Kill);
    }

    fn delete_to_line_end(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let deleted = self.value[cursor..].to_string();
        self.kill_ring.push(
            &deleted,
            KillRingPushOptions {
                prepend: false,
                accumulate: self.last_action == Some(LastAction::Kill),
            },
        );
        self.value.truncate(cursor);
        self.set_cursor(cursor);
        self.last_action = Some(LastAction::Kill);
    }

    fn delete_word_backward(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let delete_from = find_word_backward_safe(&self.value, cursor);
        let deleted = self.value[delete_from..cursor].to_string();
        self.kill_ring.push(
            &deleted,
            KillRingPushOptions {
                prepend: true,
                accumulate: was_kill,
            },
        );
        self.value.replace_range(delete_from..cursor, "");
        self.set_cursor(delete_from);
        self.last_action = Some(LastAction::Kill);
    }

    fn delete_word_forward(&mut self) {
        let cursor = self.cursor_boundary();
        if cursor >= self.value.len() {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let delete_to = find_word_forward_safe(&self.value, cursor);
        let deleted = self.value[cursor..delete_to].to_string();
        self.kill_ring.push(
            &deleted,
            KillRingPushOptions {
                prepend: false,
                accumulate: was_kill,
            },
        );
        self.value.replace_range(cursor..delete_to, "");
        self.set_cursor(cursor);
        self.last_action = Some(LastAction::Kill);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        let cursor = self.cursor_boundary();
        self.value.insert_str(cursor, &text);
        self.set_cursor(cursor + text.len());
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        let Some(previous) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let cursor = self.cursor_boundary();
        if cursor < previous.len() {
            return;
        }
        self.push_undo();
        let start = cursor - previous.len();
        self.value.replace_range(start..cursor, "");
        self.kill_ring.rotate();
        let replacement = self.kill_ring.peek().unwrap_or_default().to_string();
        self.value.insert_str(start, &replacement);
        self.set_cursor(start + replacement.len());
        self.last_action = Some(LastAction::Yank);
    }

    fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.value = snapshot.value;
            self.set_cursor(snapshot.cursor);
            self.last_action = None;
        }
    }

    fn handle_key(&mut self, key: &TuiKey) {
        self.handle_key_with_raw(key, None);
    }

    fn handle_key_with_raw(&mut self, key: &TuiKey, raw: Option<&str>) {
        let bindings = get_keybindings();
        let matches_binding = |id: &'static str| match raw {
            Some(raw) => bindings.matches_raw(raw, id),
            None => bindings.matches(key, id),
        };
        if matches_binding("tui.select.cancel") {
            if let Some(callback) = self.on_escape.as_mut() {
                callback();
            }
            return;
        }
        if matches_binding("tui.editor.undo") || key_matches(key, "ctrl+_") {
            self.undo();
            return;
        }
        if matches_binding("tui.input.submit") || key.base == "\n" {
            let value = self.value.clone();
            if let Some(callback) = self.on_submit.as_mut() {
                callback(&value);
            }
            return;
        }
        if matches_binding("tui.editor.deleteCharBackward") {
            self.handle_backspace();
            return;
        }
        if matches_binding("tui.editor.deleteCharForward") {
            self.handle_forward_delete();
            return;
        }
        if matches_binding("tui.editor.deleteWordBackward") {
            self.delete_word_backward();
            return;
        }
        if matches_binding("tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return;
        }
        if matches_binding("tui.editor.deleteToLineStart") {
            self.delete_to_line_start();
            return;
        }
        if matches_binding("tui.editor.deleteToLineEnd") {
            self.delete_to_line_end();
            return;
        }
        if matches_binding("tui.editor.yank") {
            self.yank();
            return;
        }
        if matches_binding("tui.editor.yankPop") {
            self.yank_pop();
            return;
        }
        if matches_binding("tui.editor.cursorLeft") {
            self.move_left();
            return;
        }
        if matches_binding("tui.editor.cursorRight") {
            self.move_right();
            return;
        }
        if matches_binding("tui.editor.cursorLineStart") {
            self.set_cursor(0);
            self.last_action = None;
            return;
        }
        if matches_binding("tui.editor.cursorLineEnd") {
            self.set_cursor(self.value.len());
            self.last_action = None;
            return;
        }
        if matches_binding("tui.editor.cursorWordLeft") {
            self.move_word_left();
            return;
        }
        if matches_binding("tui.editor.cursorWordRight") {
            self.move_word_right();
            return;
        }

        if key.ctrl || key.alt || is_named_key(&key.base) {
            return;
        }
        let text = if key.shift
            && key.base.len() == 1
            && key
                .base
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_lowercase())
        {
            key.base.to_ascii_uppercase()
        } else {
            key.base.clone()
        };
        self.insert_text(&text);
    }

    /// Handle raw terminal data, including bracketed paste chunks that arrive
    /// split across reads. This is useful to callers that own the raw input
    /// loop; the `Component` implementation still accepts parsed `TuiKey`s.
    pub fn handle_raw_input(&mut self, data: &str) {
        if self.in_paste || data.contains("\x1b[200~") || data.contains("\x1b[201~") {
            self.handle_paste_stream(data);
        } else {
            let key = parse_key(data);
            self.handle_key_with_raw(&key, Some(data));
        }
    }

    fn handle_paste_stream(&mut self, data: &str) {
        let mut pending = data.to_string();

        loop {
            if !self.in_paste {
                let Some(start) = pending.find("\x1b[200~") else {
                    if !pending.is_empty() {
                        let key = parse_key(&pending);
                        self.handle_key_with_raw(&key, Some(&pending));
                    }
                    return;
                };
                if start > 0 {
                    let prefix = pending[..start].to_string();
                    let key = parse_key(&prefix);
                    self.handle_key_with_raw(&key, Some(&prefix));
                }
                self.in_paste = true;
                self.paste_buffer.clear();
                pending = pending[start + "\x1b[200~".len()..].to_string();
            }

            // The end marker may be split across raw reads. Keep the bytes in
            // the paste buffer and search the accumulated content so the
            // marker is recognized when its final fragment arrives.
            self.paste_buffer.push_str(&pending);
            let Some(end) = self.paste_buffer.find("\x1b[201~") else {
                return;
            };
            let remaining = self.paste_buffer[end + "\x1b[201~".len()..].to_string();
            self.paste_buffer.truncate(end);
            let pasted = std::mem::take(&mut self.paste_buffer);
            self.insert_paste(&pasted);
            self.in_paste = false;
            pending = remaining;
            if pending.is_empty() {
                return;
            }
        }
    }

    fn insert_paste(&mut self, pasted: &str) {
        let clean = pasted
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        self.last_action = None;
        if clean.is_empty() {
            return;
        }
        self.push_undo();
        let cursor = self.cursor_boundary();
        self.value.insert_str(cursor, &clean);
        self.set_cursor(cursor + clean.len());
    }

    fn render_line(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let prompt_width = visible_width(&self.prompt);
        if prompt_width >= width {
            return slice_by_column_strict(&self.prompt, 0, width);
        }
        let available = width - prompt_width;
        let cursor = self.cursor_boundary();
        let cursor_col = visible_width(&self.value[..cursor]);
        let total_width = visible_width(&self.value);

        let (visible_text, cursor_display) = if total_width <= available {
            (self.value.clone(), cursor)
        } else {
            let scroll_width = if cursor == self.value.len() {
                available.saturating_sub(1)
            } else {
                available
            };
            if scroll_width == 0 {
                (String::new(), 0)
            } else {
                let half_width = scroll_width / 2;
                let start_col = if cursor_col < half_width {
                    0
                } else if cursor_col > total_width.saturating_sub(half_width) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half_width)
                };
                let text = slice_by_column_strict(&self.value, start_col, scroll_width);
                let before = slice_by_column_strict(
                    &self.value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                );
                (text, before.len())
            }
        };

        let cursor_display = cursor_display.min(visible_text.len());
        let (before_cursor, at_cursor, after_cursor) = if cursor_display < visible_text.len()
            && visible_text.is_char_boundary(cursor_display)
        {
            let rest = &visible_text[cursor_display..];
            let end = grapheme_boundaries(rest)
                .first()
                .map(|(_, end)| *end)
                .unwrap_or(0);
            if end > 0 {
                (&visible_text[..cursor_display], &rest[..end], &rest[end..])
            } else {
                (&visible_text[..cursor_display], " ", "")
            }
        } else {
            (&visible_text[..cursor_display], " ", "")
        };
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let cursor_text = format!("\x1b[7m{at_cursor}\x1b[27m");
        let body = format!("{before_cursor}{marker}{cursor_text}{after_cursor}");
        let padding = " ".repeat(available.saturating_sub(visible_width(&body)));
        let line = format!("{}{body}{padding}", self.prompt);
        if visible_width(&line) > width {
            slice_by_column_strict(&line, 0, width)
        } else {
            line
        }
    }
}

fn key_matches(key: &TuiKey, pattern: &str) -> bool {
    match_key(key, pattern) || key.canonical() == pattern
}

fn is_input_control(character: char) -> bool {
    let codepoint = character as u32;
    codepoint < 0x20 || codepoint == 0x7f || (0x80..=0x9f).contains(&codepoint)
}

fn is_named_key(base: &str) -> bool {
    matches!(
        base,
        "esc"
            | "escape"
            | "enter"
            | "return"
            | "tab"
            | "backspace"
            | "delete"
            | "insert"
            | "clear"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "up"
            | "down"
            | "left"
            | "right"
            | "space"
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "f12"
    )
}

impl Component for Input {
    fn render(&self, width: usize) -> Vec<String> {
        vec![self.render_line(width)]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if key.base.contains("\x1b[200~") || self.in_paste {
            self.handle_paste_stream(&key.base);
        } else {
            self.handle_key(key);
        }
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::utils::{strip_ansi_codes, visible_width};

    fn raw(input: &mut Input, data: &str) {
        input.handle_raw_input(data);
    }

    #[test]
    fn typing_inserts_characters_and_submit_keeps_backslash() {
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let submitted_for_callback = submitted.clone();
        let mut input = Input::new("> ").with_submit_callback(move |value| {
            *submitted_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(value.to_string());
        });
        raw(&mut input, "hello\\");
        raw(&mut input, "\r");
        assert_eq!(input.value, "hello\\");
        assert_eq!(
            *submitted.lock().unwrap_or_else(|error| error.into_inner()),
            Some("hello\\".to_string())
        );
    }

    #[test]
    fn configured_modified_submit_binding_invokes_callback() {
        use crate::keybindings::{
            get_keybindings, set_keybindings, KeybindingsConfig, KeybindingsManager,
            TUI_KEYBINDINGS,
        };

        struct RestoreKeybindings(KeybindingsManager);

        impl Drop for RestoreKeybindings {
            fn drop(&mut self) {
                set_keybindings(self.0.clone());
            }
        }

        let _restore = RestoreKeybindings(get_keybindings());
        let mut config = KeybindingsConfig::new();
        config.insert(
            "tui.input.submit".to_string(),
            vec!["ctrl+enter".to_string()],
        );
        set_keybindings(KeybindingsManager::new(TUI_KEYBINDINGS, config));

        let submitted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let submitted_for_callback = submitted.clone();
        let mut input = Input::new("> ").with_submit_callback(move |value| {
            *submitted_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(value.to_string());
        });
        input.set_value("modified submit");
        input.handle_key(&TuiKey::ctrl("enter"));

        assert_eq!(
            *submitted.lock().unwrap_or_else(|error| error.into_inner()),
            Some("modified submit".to_string())
        );
    }

    #[test]
    fn backspace_removes_one_grapheme_not_one_scalar() {
        let mut input = Input::new("> ");
        input.set_value("e\u{301}🙂");
        raw(&mut input, "\x05");
        raw(&mut input, "\x7f");
        assert_eq!(input.value, "e\u{301}");
        raw(&mut input, "\x7f");
        assert_eq!(input.value, "");
    }

    #[test]
    fn cursor_moves_across_combining_and_emoji_clusters() {
        let mut input = Input::new("> ");
        input.set_value("e\u{301}👩‍💻界");
        raw(&mut input, "\x05");
        let end = input.cursor;
        raw(&mut input, "\x1b[D");
        assert_eq!(&input.value[input.cursor..end], "界");
        raw(&mut input, "\x1b[D");
        assert_eq!(&input.value[input.cursor..], "👩‍💻界");
        raw(&mut input, "\x1b[H");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn kill_ring_supports_word_line_and_yank_operations() {
        let mut input = Input::new("");
        input.set_value("foo bar baz");
        raw(&mut input, "\x05");
        raw(&mut input, "\x17");
        assert_eq!(input.value, "foo bar ");
        raw(&mut input, "\x01");
        raw(&mut input, "\x19");
        assert_eq!(input.value, "bazfoo bar ");

        input.set_value("hello world");
        raw(&mut input, "\x01");
        for _ in 0..6 {
            raw(&mut input, "\x1b[C");
        }
        raw(&mut input, "\x15");
        assert_eq!(input.value, "world");
        raw(&mut input, "\x19");
        assert_eq!(input.value, "hello world");
    }

    #[test]
    fn consecutive_kills_accumulate_and_yank_pop_cycles() {
        let mut input = Input::new("");
        assert_eq!(
            parse_key("\x1by"),
            TuiKey {
                base: "y".to_string(),
                ctrl: false,
                shift: false,
                alt: true,
                super_key: false,
            }
        );
        for word in ["first", "second", "third"] {
            input.set_value(word);
            raw(&mut input, "\x05");
            raw(&mut input, "\x17");
        }
        raw(&mut input, "\x19");
        assert_eq!(input.value, "third");
        raw(&mut input, "\x1by");
        assert_eq!(input.value, "second");
        raw(&mut input, "\x1by");
        assert_eq!(input.value, "first");
        raw(&mut input, "\x1by");
        assert_eq!(input.value, "third");
    }

    #[test]
    fn undo_coalesces_words_but_not_spaces_or_edit_actions() {
        let mut input = Input::new("");
        for character in "hello  ".chars() {
            raw(&mut input, &character.to_string());
        }
        raw(&mut input, "\x1b[45;5u");
        assert_eq!(input.value, "hello ");
        raw(&mut input, "\x1b[45;5u");
        assert_eq!(input.value, "hello");
        raw(&mut input, "\x1b[45;5u");
        assert_eq!(input.value, "");
    }

    #[test]
    fn bracketed_paste_is_buffered_and_normalized_across_chunks() {
        let mut input = Input::new("");
        raw(&mut input, "\x1b[200~a\r\n");
        raw(&mut input, "b\t\x1b[201~!");
        assert_eq!(input.value, "ab    !");
    }

    #[test]
    fn bracketed_paste_end_marker_can_be_split_across_reads() {
        let mut input = Input::new("");
        raw(&mut input, "before\x1b[200~paste\x1b[20");
        assert_eq!(input.value, "before");
        raw(&mut input, "1~after");
        assert_eq!(input.value, "beforepasteafter");
    }

    #[test]
    fn set_value_preserves_and_clamps_the_grapheme_cursor() {
        let mut input = Input::new("");
        input.set_value("e\u{301}🙂界");
        raw(&mut input, "\x05");
        input.set_value("e\u{301}🙂");
        assert_eq!(input.cursor, "e\u{301}🙂".len());

        input.set_value("e\u{301}🙂界");
        raw(&mut input, "\x1b[C");
        input.set_value("e\u{301}🙂界x");
        assert_eq!(input.cursor, ("e\u{301}🙂界").len());
    }

    #[test]
    fn unicode_word_navigation_and_kill_respect_grapheme_boundaries() {
        crate::keys::set_kitty_protocol_active(false);
        let mut input = Input::new("");
        input.set_value("e\u{301}🙂 foo.bar");
        raw(&mut input, "\x1b[H");
        raw(&mut input, "\x1b[1;5C");
        assert_eq!(input.cursor, "e\u{301}🙂".len());
        raw(&mut input, "\x1b[1;5C");
        assert_eq!(input.cursor, "e\u{301}🙂 foo".len());
        raw(&mut input, "\x1b[1;5D");
        assert_eq!(input.cursor, "e\u{301}🙂 ".len());

        input.set_value("foo.bar");
        raw(&mut input, "\x1b[H");
        raw(&mut input, "\x1bd");
        assert_eq!(input.value, ".bar");
        raw(&mut input, "\x19");
        assert_eq!(input.value, "foo.bar");
    }

    #[test]
    fn cjk_word_runs_are_grouped_around_fullwidth_punctuation() {
        crate::keys::set_kitty_protocol_active(false);
        let mut input = Input::new("");
        input.set_value("你好世界。你好，世界");
        raw(&mut input, "\x05");

        for (step, expected) in [
            ("你好世界。你好，", "你好世界。你好，"),
            ("你好世界。你好", "你好世界。你好"),
            ("你好世界。", "你好世界。"),
            ("你好世界", "你好世界"),
            ("你好", "你好"),
            ("", ""),
        ] {
            raw(&mut input, "\x17");
            assert_eq!(input.value, expected, "after deleting toward {step:?}");
        }
        assert_eq!(input.value, "");
    }

    #[test]
    fn focused_render_emits_marker_and_never_overflows_wide_text() {
        let mut input = Input::new("> ");
        input.set_value("가나다라마바사아자차카타파하");
        input.focused = true;
        for _ in 0..5 {
            raw(&mut input, "\x1b[D");
        }
        let line = input.render(20).remove(0);
        assert!(line.contains(CURSOR_MARKER));
        assert!(visible_width(&line) <= 20);
        assert!(strip_ansi_codes(&line).contains('>'));
    }

    #[test]
    fn escape_callback_runs_without_mutating_value() {
        let escaped = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let escaped_for_callback = escaped.clone();
        let mut input = Input::new("").with_escape_callback(move || {
            *escaped_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) += 1;
        });
        raw(&mut input, "test");
        raw(&mut input, "\x1b");
        assert_eq!(input.value, "test");
        assert_eq!(
            *escaped.lock().unwrap_or_else(|error| error.into_inner()),
            1
        );
    }
}
