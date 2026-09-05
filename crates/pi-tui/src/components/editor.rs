//! Editor component — port of `packages/tui/src/components/editor.ts`.
//!
//! Multi-line text editing with grapheme-aware cursor movement, word
//! navigation, Emacs-style kill/yank, undo coalescing, prompt history, paste
//! markers for large pastes, and autocomplete integration.
//!
//! Documented divergence: the upstream component uses the platform
//! `Intl.Segmenter` (dictionary-based) and an async, debounced autocomplete
//! provider. This port uses a deterministic grapheme/word segmenter and a
//! synchronous autocomplete provider (the interactive loop calls
//! `drain_autocomplete_tick` from its event loop).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::autocomplete::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};
use crate::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use crate::keybindings::get_keybindings;
use crate::keys::{is_key_release, matches_raw_key, parse_key, TuiKey};
use crate::kill_ring::{KillRing, KillRingPushOptions};
use crate::tui::Component;
use crate::undo_stack::UndoStack;
use crate::utils::{grapheme_boundaries, next_grapheme_boundary, slice_with_width, visible_width};
use crate::word_navigation::{
    find_word_backward, find_word_forward, segment_text, Segment, WordNavigationOptions,
};

/// Regex matching paste markers like `[paste #1 +123 lines]`.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn paste_marker_regex() -> regex::Regex {
    regex::Regex::new(r"\[paste #(\d+)( (\+\d+ lines|\d+ chars))?\]").unwrap()
}

fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10
        && paste_marker_regex()
            .find(segment)
            .map(|matched| matched.as_str() == segment)
            .unwrap_or(false)
}

/// A chunk of text for word-wrap layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Segment text into grapheme-like units (each with index/byte width).
fn grapheme_segments(text: &str) -> Vec<Segment> {
    grapheme_boundaries(text)
        .into_iter()
        .map(|(start, end)| Segment {
            segment: text[start..end].to_string(),
            index: start,
            is_word_like: true,
        })
        .collect()
}

/// Preserve word-navigation semantics without allowing the word segmenter to
/// split a combining or emoji grapheme into separate cursor units.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn grapheme_safe_word_segments(text: &str) -> Vec<Segment> {
    let graphemes = grapheme_boundaries(text);
    let mut merged: Vec<Segment> = Vec::new();
    for segment in segment_text(text) {
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

    // Match the pinned editor tests' `Intl.Segmenter` behavior: adjacent CJK
    // script characters form dictionary-sized word-like units, but ideographic punctuation
    // and unrelated graphemes remain separate segments.  The Rust fallback
    // segmenter emits one CJK scalar at a time and can attach non-ASCII
    // punctuation to a neighboring word, so repair those boundaries first.
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
                && cjk_runs_join(&previous.segment, &segment.segment)
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

/// Script-aware CJK run grouping for word navigation.
///
/// Same-script-class runs join; mixed Han/phonetic runs split at the Han
/// boundary. Han runs stay pair-capped (pinned `你好|世界` behavior);
/// phonetic-script runs (Hiragana/Katakana/Hangul/Bopomofo) absorb maximally
/// (pinned `こんにちは` whole-word behavior). See
/// `word_navigation::cjk_segment_contains_han` for the calibration note.
fn cjk_runs_join(previous: &str, segment: &str) -> bool {
    let previous_han = crate::word_navigation::cjk_segment_contains_han(previous);
    if previous_han != crate::word_navigation::cjk_segment_contains_han(segment) {
        return false;
    }
    if previous_han {
        return grapheme_boundaries(previous).len() < 2;
    }
    true
}

/// Clamp an offset to a UTF-8 character boundary without ever indexing the
/// string at an invalid byte position.
fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Clamp an editor cursor to the start of the grapheme containing it.  The
/// public autocomplete contract supplies byte offsets, so this remains a
/// necessary defensive boundary even though the built-in movement paths are
/// already grapheme-aware.
fn floor_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    grapheme_segments(text)
        .into_iter()
        .find_map(|segment| {
            (offset > segment.index && offset < segment.index + segment.segment.len())
                .then_some(segment.index)
        })
        .unwrap_or(offset)
}

/// Convert a terminal-cell column within a byte-bounded line segment back to
/// a valid UTF-8 offset.  If a requested column lands in the middle of a wide
/// grapheme, the cursor snaps to that grapheme's start rather than splitting
/// it and panicking during the next render.
fn byte_offset_for_visual_column(text: &str, start: usize, columns: usize) -> usize {
    let start = floor_char_boundary(text, start);
    let suffix = &text[start..];
    let mut consumed = 0usize;
    for segment in grapheme_segments(suffix) {
        let width = visible_width(&segment.segment);
        if consumed.saturating_add(width) > columns {
            return start + segment.index;
        }
        consumed = consumed.saturating_add(width);
        if consumed == columns {
            return start + segment.index + segment.segment.len();
        }
    }
    text.len()
}

/// Return the terminal-cell column between two valid byte offsets.
fn visual_column_between(text: &str, start: usize, end: usize) -> usize {
    let start = floor_char_boundary(text, start);
    let end = floor_char_boundary(text, end.max(start));
    visible_width(&text[start..end])
}

/// Validate the optional segmentation supplied to `word_wrap_line`.  The
/// function is public and its segment type is public, so callers can provide
/// stale or character-indexed offsets. Falling back to local segmentation is
/// safer than allowing those offsets to reach a string slice.
fn valid_wrap_segments(line: &str, segments: &[Segment]) -> bool {
    let mut offset = 0usize;
    for segment in segments {
        if segment.index != offset || !line.is_char_boundary(segment.index) {
            return false;
        }
        let Some(end) = offset.checked_add(segment.segment.len()) else {
            return false;
        };
        if end > line.len()
            || !line.is_char_boundary(end)
            || line.get(offset..end) != Some(segment.segment.as_str())
        {
            return false;
        }
        offset = end;
    }
    offset == line.len()
}

/// Segment with paste-marker awareness: markers whose ID exists in
/// `valid_ids` are merged into single atomic segments.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn segment_with_markers(
    text: &str,
    base: &[Segment],
    valid_ids: &std::collections::HashSet<usize>,
) -> Vec<Segment> {
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return base.to_vec();
    }
    let re = paste_marker_regex();
    let mut markers: Vec<(usize, usize)> = Vec::new();
    for m in re.captures_iter(text) {
        let id: usize = m.get(1).unwrap().as_str().parse().unwrap_or(0);
        if !valid_ids.contains(&id) {
            continue;
        }
        let full = m.get(0).unwrap();
        markers.push((full.start(), full.end()));
    }
    if markers.is_empty() {
        return base.to_vec();
    }

    let mut result: Vec<Segment> = Vec::new();
    let mut marker_idx = 0usize;
    for seg in base {
        while marker_idx < markers.len() && markers[marker_idx].1 <= seg.index {
            marker_idx += 1;
        }
        let marker = markers.get(marker_idx).copied();
        if let Some((mstart, mend)) = marker {
            if seg.index >= mstart && seg.index < mend {
                if seg.index == mstart {
                    result.push(Segment {
                        segment: text[mstart..mend].to_string(),
                        index: mstart,
                        is_word_like: true,
                    });
                }
                continue;
            }
        }
        result.push(seg.clone());
    }
    result
}

/// Split a line into word-wrapped chunks (upstream `wordWrapLine`).
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    pre_segmented: Option<&[Segment]>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    let line_width = visible_width(line);
    if line_width <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: line.len(),
        }];
    }

    let segments: Vec<Segment> = match pre_segmented {
        Some(s) if valid_wrap_segments(line, s) => s.to_vec(),
        _ => grapheme_segments(line),
    };

    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    let mut wrap_opp_index: isize = -1;
    let mut wrap_opp_width = 0usize;

    for i in 0..segments.len() {
        let seg = &segments[i];
        let grapheme = &seg.segment;
        let g_width = visible_width(grapheme);
        let char_index = seg.index;
        let is_ws = !is_paste_marker(grapheme)
            && grapheme
                .chars()
                .next()
                .map(|c| c.is_whitespace())
                .unwrap_or(false);

        if current_width + g_width > max_width {
            if wrap_opp_index >= 0 && current_width - wrap_opp_width + g_width <= max_width {
                let woi = wrap_opp_index as usize;
                chunks.push(TextChunk {
                    text: line[chunk_start..woi].to_string(),
                    start_index: chunk_start,
                    end_index: woi,
                });
                chunk_start = woi;
                current_width -= wrap_opp_width;
            } else if chunk_start < char_index {
                chunks.push(TextChunk {
                    text: line[chunk_start..char_index].to_string(),
                    start_index: chunk_start,
                    end_index: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = -1;
        }

        if g_width > max_width {
            let sub = word_wrap_line(grapheme, max_width, None);
            for sc in sub[..sub.len().saturating_sub(1)].iter() {
                chunks.push(TextChunk {
                    text: sc.text.clone(),
                    start_index: char_index + sc.start_index,
                    end_index: char_index + sc.end_index,
                });
            }
            if let Some(last) = sub.last() {
                chunk_start = char_index + last.start_index;
                current_width = visible_width(&last.text);
            }
            wrap_opp_index = -1;
            continue;
        }

        current_width += g_width;

        let next = segments.get(i + 1);
        if is_ws {
            if let Some(nextseg) = next {
                if is_paste_marker(&nextseg.segment)
                    || !nextseg
                        .segment
                        .chars()
                        .next()
                        .map(|c| c.is_whitespace())
                        .unwrap_or(false)
                {
                    wrap_opp_index = nextseg.index as isize;
                    wrap_opp_width = current_width;
                }
            }
        } else if let Some(nextseg) = next {
            let next_ws = nextseg.segment.chars().next().map(|c| c.is_whitespace());
            if next_ws == Some(false) {
                let is_cjk = !is_paste_marker(grapheme) && is_cjk_segment(grapheme);
                let next_is_cjk =
                    !is_paste_marker(&nextseg.segment) && is_cjk_segment(&nextseg.segment);
                if is_cjk || next_is_cjk {
                    wrap_opp_index = nextseg.index as isize;
                    wrap_opp_width = current_width;
                }
            }
        }
    }

    chunks.push(TextChunk {
        text: line[chunk_start..].to_string(),
        start_index: chunk_start,
        end_index: line.len(),
    });
    chunks
}

fn is_cjk_segment(seg: &str) -> bool {
    seg.chars()
        .next()
        .map(crate::word_navigation::is_cjk_char)
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq)]
struct EditorState {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

impl EditorState {
    fn empty() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct EditorSnapshot {
    state: EditorState,
    pastes: HashMap<usize, String>,
    paste_counter: usize,
}

#[derive(Debug, Clone)]
struct LayoutLine {
    text: String,
    has_cursor: bool,
    cursor_pos: Option<usize>,
}

/// Theme for the editor border.
pub struct EditorTheme {
    pub border_color: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl std::fmt::Debug for EditorTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorTheme").finish()
    }
}

/// Default theme: plain border (no ANSI decoration) — for tests.
pub fn plain_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Arc::new(|s| s.to_string()),
    }
}

pub struct EditorOptions {
    pub padding_x: usize,
    pub autocomplete_max_visible: usize,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self {
            padding_x: 0,
            autocomplete_max_visible: 5,
        }
    }
}

fn create_scroll_border(direction: &str, hidden_line_count: usize, width: usize) -> String {
    let available_width = width;
    let indicator = format!("─── {direction} {hidden_line_count} more ");
    let indicator_w = visible_width(&indicator);
    if indicator_w <= available_width {
        return format!("{}{}", indicator, "─".repeat(available_width - indicator_w));
    }
    let ellipsis = slice_with_width("...", available_width);
    let indicator_width = available_width.saturating_sub(visible_width(&ellipsis));
    let sliced = slice_with_width(&indicator, indicator_width);
    format!("{sliced}{ellipsis}")
}

const SLASH_COMMAND_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
};

const AUTOCOMPLETE_DEBOUNCE: Duration = Duration::from_millis(20);

struct PendingAutocomplete {
    force: bool,
    explicit_tab: bool,
    generation: u64,
    due: Instant,
}

/// The editor component.
pub struct Editor {
    state: EditorState,
    pub focused: bool,
    padding_x: usize,
    last_width: AtomicUsize,
    scroll_offset: std::sync::atomic::AtomicUsize,
    pub border_color: Arc<dyn Fn(&str) -> String + Send + Sync>,
    terminal_rows: usize,

    // Autocomplete
    autocomplete_provider: Option<Box<dyn AutocompleteProvider + Send + Sync>>,
    autocomplete_trigger_characters: Vec<String>,
    autocomplete_list: Option<SelectList>,
    autocomplete_state: Option<&'static str>,
    autocomplete_prefix: String,
    autocomplete_max_visible: usize,
    autocomplete_generation: u64,
    autocomplete_pending: Option<PendingAutocomplete>,
    autocomplete_abort: Option<Arc<AtomicBool>>,

    // Paste tracking
    pastes: HashMap<usize, String>,
    paste_counter: usize,
    paste_buffer: String,
    is_in_paste: bool,

    // History
    history: Vec<String>,
    history_index: isize,
    history_draft: Option<EditorState>,

    // Kill ring
    kill_ring: KillRing,
    last_action: Option<&'static str>,

    // Jump mode
    jump_mode: Option<&'static str>,

    // Sticky column
    preferred_visual_col: Option<usize>,
    snapped_from_cursor_col: Option<usize>,

    // Undo
    undo_stack: UndoStack<EditorSnapshot>,

    pub disable_submit: bool,
    /// Set when a submit happened (drained by the interactive loop).
    submit_pending: Option<String>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("state", &self.state)
            .finish()
    }
}

impl Editor {
    pub fn new(terminal_rows: usize, theme: EditorTheme, options: EditorOptions) -> Self {
        let padding_x = options.padding_x;
        let autocomplete_max_visible = options.autocomplete_max_visible.clamp(3, 20);
        let border_color = theme.border_color.clone();
        Self {
            state: EditorState::empty(),
            focused: false,
            padding_x,
            last_width: AtomicUsize::new(80),
            scroll_offset: std::sync::atomic::AtomicUsize::new(0),
            border_color,
            terminal_rows,
            autocomplete_provider: None,
            autocomplete_trigger_characters: vec!["@".to_string(), "#".to_string()],
            autocomplete_list: None,
            autocomplete_state: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible,
            autocomplete_generation: 0,
            autocomplete_pending: None,
            autocomplete_abort: None,
            pastes: HashMap::new(),
            paste_counter: 0,
            paste_buffer: String::new(),
            is_in_paste: false,
            history: Vec::new(),
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: UndoStack::new(),
            disable_submit: false,
            submit_pending: None,
        }
    }

    pub fn set_terminal_rows(&mut self, rows: usize) {
        self.terminal_rows = rows;
    }

    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
    }

    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        self.autocomplete_max_visible = max_visible.clamp(3, 20);
    }

    pub fn set_autocomplete_provider(
        &mut self,
        provider: Box<dyn AutocompleteProvider + Send + Sync>,
    ) {
        self.cancel_autocomplete();
        let triggers = provider.trigger_characters();
        self.autocomplete_provider = Some(provider);
        self.set_autocomplete_trigger_characters(triggers);
    }

    fn valid_paste_ids(&self) -> std::collections::HashSet<usize> {
        self.pastes.keys().copied().collect()
    }

    fn segment(&self, text: &str, mode: &str) -> Vec<Segment> {
        let base = if mode == "word" {
            grapheme_safe_word_segments(text)
        } else {
            grapheme_segments(text)
        };
        segment_with_markers(text, &base, &self.valid_paste_ids())
    }

    // ------------------------------------------------------------------ history

    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.first().map(|h| h.as_str()) == Some(trimmed) {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        if self.history.len() > 100 {
            self.history.pop();
        }
    }

    fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    fn is_on_first_visual_line(&self) -> bool {
        let visual = self.build_visual_line_map(self.last_width.load(Ordering::Relaxed));
        self.find_current_visual_line(&visual) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let visual = self.build_visual_line_map(self.last_width.load(Ordering::Relaxed));
        self.find_current_visual_line(&visual) == visual.len() - 1
    }

    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index - direction; // Up(-1) increases index
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index == -1 && new_index >= 0 {
            self.push_undo_snapshot();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = new_index;

        if self.history_index == -1 {
            let draft = self.history_draft.take();
            if let Some(draft) = draft {
                self.state = draft;
                self.preferred_visual_col = None;
                self.snapped_from_cursor_col = None;
                self.scroll_offset
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            } else {
                self.set_text_internal("", "end");
            }
        } else {
            let entry = self
                .history
                .get(self.history_index as usize)
                .cloned()
                .unwrap_or_default();
            let placement = if direction == -1 { "start" } else { "end" };
            self.set_text_internal(&entry, placement);
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }

    fn set_text_internal(&mut self, text: &str, cursor_placement: &str) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        self.state.lines = lines;
        self.state.cursor_line = if cursor_placement == "start" {
            0
        } else {
            self.state.lines.len() - 1
        };
        let col = if cursor_placement == "start" {
            0
        } else {
            self.state.lines[self.state.cursor_line].len()
        };
        self.set_cursor_col(col);
        self.scroll_offset
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    // ------------------------------------------------------------------ text access

    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    /// Check whether the editor contains no user text without allocating the
    /// complete multiline draft. Interactive mode uses this on every control
    /// key boundary, so keeping the query scalar avoids cloning a long draft
    /// while preserving `get_text` for callers that need the value.
    pub fn is_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    /// Check the first non-whitespace character without materializing the
    /// complete draft. This is used for the `!` bash composer border state.
    pub fn starts_with_non_whitespace(&self, character: char) -> bool {
        self.state
            .lines
            .first()
            .is_some_and(|line| line.trim_start().starts_with(character))
    }

    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn expand_paste_markers(&self, text: &str) -> String {
        let mut ids: Vec<usize> = self.pastes.keys().copied().collect();
        ids.sort_by(|a, b| b.cmp(a)); // replace larger ids first to avoid clobbering
        let mut result = text.to_string();
        for id in ids {
            let marker = format!(r"\[paste #{id}( (\+\d+ lines|\d+ chars))?\]");
            let marker_re = regex::Regex::new(&marker).unwrap();
            if let Some(content) = self.pastes.get(&id) {
                result = marker_re.replace_all(&result, content.as_str()).to_string();
            }
        }
        result
    }

    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.get_text())
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = self.normalize_text(text);
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, "end");
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    fn normalize_text(&self, text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = self.normalize_text(text);
        let inserted_lines: Vec<&str> = normalized.split('\n').collect();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        let cursor_col = floor_grapheme_boundary(&current_line, self.state.cursor_col);
        let before_cursor = current_line[..cursor_col].to_string();
        let after_cursor = current_line[cursor_col..].to_string();

        if inserted_lines.len() == 1 {
            self.state.lines[self.state.cursor_line] =
                format!("{before_cursor}{normalized}{after_cursor}");
            self.set_cursor_col(cursor_col + normalized.len());
        } else {
            let mut new_lines: Vec<String> = Vec::new();
            new_lines.extend(self.state.lines[..self.state.cursor_line].iter().cloned());
            new_lines.push(format!("{before_cursor}{}", inserted_lines[0]));
            for mid in inserted_lines
                .iter()
                .skip(1)
                .take(inserted_lines.len().saturating_sub(2))
            {
                new_lines.push(mid.to_string());
            }
            new_lines.push(format!(
                "{}{after_cursor}",
                inserted_lines[inserted_lines.len() - 1]
            ));
            new_lines.extend(
                self.state.lines[self.state.cursor_line + 1..]
                    .iter()
                    .cloned(),
            );
            self.state.lines = new_lines;
            self.state.cursor_line += inserted_lines.len() - 1;
            self.set_cursor_col(inserted_lines[inserted_lines.len() - 1].len());
        }
    }
}

impl Editor {
    // ------------------------------------------------------------------ render

    fn render_editor(&self, width: usize) -> Vec<String> {
        let max_padding = if width > 1 { (width - 1) / 2 } else { 0 };
        let padding_x = self.padding_x.min(max_padding);
        let content_width = std::cmp::max(1, width.saturating_sub(padding_x * 2));
        let layout_width = std::cmp::max(
            1,
            content_width.saturating_sub(if padding_x > 0 { 0 } else { 1 }),
        );
        self.last_width.store(layout_width, Ordering::Relaxed);

        let horizontal = (self.border_color)("─");
        let layout_lines = self.layout_text(layout_width);

        let terminal_rows = self.terminal_rows.max(5);
        let max_visible_lines = std::cmp::max(5, terminal_rows * 3 / 10);

        let cursor_line_index = layout_lines.iter().position(|l| l.has_cursor).unwrap_or(0);
        let scroll = self
            .scroll_offset
            .load(std::sync::atomic::Ordering::Relaxed);
        if cursor_line_index < scroll {
            self.scroll_offset
                .store(cursor_line_index, std::sync::atomic::Ordering::Relaxed);
        } else if cursor_line_index >= scroll + max_visible_lines {
            self.scroll_offset.store(
                cursor_line_index - max_visible_lines + 1,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let max_scroll_offset = layout_lines.len().saturating_sub(max_visible_lines);
        self.scroll_offset.store(
            self.scroll_offset
                .load(std::sync::atomic::Ordering::Relaxed)
                .min(max_scroll_offset),
            std::sync::atomic::Ordering::Relaxed,
        );

        let scroll = self
            .scroll_offset
            .load(std::sync::atomic::Ordering::Relaxed);
        let visible_lines = &layout_lines
            [scroll..scroll + max_visible_lines.min(layout_lines.len().saturating_sub(scroll))];

        let mut result: Vec<String> = Vec::new();
        let left_padding = " ".repeat(padding_x);
        let right_padding = left_padding.clone();

        let scroll = self
            .scroll_offset
            .load(std::sync::atomic::Ordering::Relaxed);
        if scroll > 0 {
            result.push((self.border_color)(&create_scroll_border(
                "↑", scroll, width,
            )));
        } else {
            result.push(horizontal.repeat(width));
        }

        let emit_cursor_marker = self.focused;
        for layout_line in visible_lines {
            let mut display_text = layout_line.text.clone();
            let mut line_visible_width = visible_width(&layout_line.text);
            let mut cursor_in_padding = false;

            if layout_line.has_cursor {
                if let Some(cursor_pos) = layout_line.cursor_pos {
                    let before = display_text[..cursor_pos.min(display_text.len())].to_string();
                    let after = display_text[cursor_pos.min(display_text.len())..].to_string();

                    // Hardware cursor marker for IME positioning.
                    let marker = if emit_cursor_marker {
                        "\x1b_pi:c\x07"
                    } else {
                        ""
                    };

                    if !after.is_empty() {
                        let gen = self.segment(&after, "grapheme");
                        let first_grapheme =
                            gen.first().map(|s| s.segment.clone()).unwrap_or_default();
                        let rest_after = after[first_grapheme.len().min(after.len())..].to_string();
                        let cursor = format!("\x1b[7m{first_grapheme}\x1b[0m");
                        display_text = format!("{before}{marker}{cursor}{rest_after}");
                    } else {
                        display_text = format!("{before}{marker}\x1b[7m \x1b[0m");
                        line_visible_width += 1;
                        if line_visible_width > content_width && padding_x > 0 {
                            cursor_in_padding = true;
                        }
                    }
                }
            }

            let padding = " ".repeat(content_width.saturating_sub(line_visible_width));
            let line_right_padding = if cursor_in_padding {
                right_padding[1..].to_string()
            } else {
                right_padding.clone()
            };
            result.push(format!(
                "{left_padding}{display_text}{padding}{line_right_padding}"
            ));
        }

        let scroll = self
            .scroll_offset
            .load(std::sync::atomic::Ordering::Relaxed);
        let lines_below = layout_lines
            .len()
            .saturating_sub((scroll + visible_lines.len()).min(layout_lines.len()));
        if lines_below > 0 {
            result.push((self.border_color)(&create_scroll_border(
                "↓",
                lines_below,
                width,
            )));
        } else {
            result.push(horizontal.repeat(width));
        }

        // Autocomplete picker
        if self.autocomplete_state.is_some() {
            if let Some(list) = &self.autocomplete_list {
                let rendered = list.render(content_width);
                for line in rendered {
                    let line_width = visible_width(&line);
                    let line_padding = " ".repeat(content_width.saturating_sub(line_width));
                    result.push(format!("{left_padding}{line}{line_padding}{right_padding}"));
                }
            }
        }

        result
    }

    fn layout_text(&self, content_width: usize) -> Vec<LayoutLine> {
        let mut layout_lines: Vec<LayoutLine> = Vec::new();

        if self.state.lines.is_empty()
            || (self.state.lines.len() == 1 && self.state.lines[0].is_empty())
        {
            layout_lines.push(LayoutLine {
                text: String::new(),
                has_cursor: true,
                cursor_pos: Some(0),
            });
            return layout_lines;
        }

        for i in 0..self.state.lines.len() {
            let line = &self.state.lines[i];
            let is_current_line = i == self.state.cursor_line;
            let line_visible_width = visible_width(line);

            if line_visible_width <= content_width {
                if is_current_line {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: true,
                        cursor_pos: Some(self.state.cursor_col.min(line.len())),
                    });
                } else {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: false,
                        cursor_pos: None,
                    });
                }
            } else {
                let segmented = self.segment(line, "grapheme");
                let chunks = word_wrap_line(line, content_width, Some(&segmented));
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let cursor_pos = self.state.cursor_col;
                    let is_last_chunk = chunk_index == chunks.len() - 1;
                    let mut has_cursor_in_chunk = false;
                    let mut adjusted_cursor_pos = 0usize;

                    if is_current_line {
                        if is_last_chunk {
                            has_cursor_in_chunk = cursor_pos >= chunk.start_index;
                            adjusted_cursor_pos = cursor_pos.saturating_sub(chunk.start_index);
                        } else {
                            has_cursor_in_chunk =
                                cursor_pos >= chunk.start_index && cursor_pos < chunk.end_index;
                            if has_cursor_in_chunk {
                                adjusted_cursor_pos = cursor_pos - chunk.start_index;
                                if adjusted_cursor_pos > chunk.text.len() {
                                    adjusted_cursor_pos = chunk.text.len();
                                }
                            }
                        }
                    }

                    if has_cursor_in_chunk {
                        layout_lines.push(LayoutLine {
                            text: chunk.text.clone(),
                            has_cursor: true,
                            cursor_pos: Some(adjusted_cursor_pos),
                        });
                    } else {
                        layout_lines.push(LayoutLine {
                            text: chunk.text.clone(),
                            has_cursor: false,
                            cursor_pos: None,
                        });
                    }
                }
            }
        }

        layout_lines
    }
}

impl Editor {
    // ------------------------------------------------------------------ input

    /// Apply a burst of printable terminal input without requiring callers to
    /// redraw once for every byte.  The terminal backend intentionally keeps
    /// control/escape sequences separate, so this method only needs to split
    /// the printable text into grapheme-like units before feeding the normal
    /// key path.  Keeping the single-key path authoritative preserves all
    /// autocomplete, history, undo, paste, and cursor semantics.
    pub fn handle_input_burst(&mut self, data: &str) {
        self.drain_autocomplete_tick();
        let segments = grapheme_segments(data);
        if data.contains('\x1b') {
            // Escape-prefixed input may be a complete terminal sequence
            // (including a bracketed-paste marker). Splitting it into
            // graphemes would destroy the protocol framing, so keep the
            // normal raw-key path authoritative for these bursts.
            self.handle_input(data);
            return;
        }
        if segments.len() <= 1 {
            if let Some(segment) = segments.first() {
                if segment
                    .segment
                    .chars()
                    .all(|character| !character.is_control())
                {
                    self.insert_character(&segment.segment);
                    return;
                }
            }
            self.handle_input(data);
            return;
        }
        for segment in segments {
            if segment
                .segment
                .chars()
                .all(|character| !character.is_control())
            {
                // `handle_input` intentionally treats a multi-byte string as
                // a key sequence. A printable grapheme burst is already
                // classified, so call the insertion primitive directly and
                // retain the complete cluster (including ZWJ/combining
                // marks).
                self.insert_character(&segment.segment);
            } else {
                // Newline and other non-escape controls still have key
                // semantics when a caller supplies a mixed burst such as
                // `"before\nafter"`.
                self.handle_input(&segment.segment);
            }
        }
    }

    pub fn handle_input(&mut self, data: &str) {
        // Keyboard map for common raw sequences is handled by the terminal
        // backend (key string); here data is a key string such as "a",
        // "enter", "ctrl+c", "up", "shift+enter".

        // Drive delayed attachment completion from the event loop. A caller
        // that owns a render loop can call `drain_autocomplete_tick` directly
        // as well; this keeps the debounce deterministic without spawning an
        // unmanaged timer thread.
        self.drain_autocomplete_tick();
        let bindings = get_keybindings();
        let parsed_key = parse_key(data);
        let matches_binding = |id: &'static str| bindings.matches_raw(data, id);

        // Character jump mode.
        if self.jump_mode.is_some() {
            let jump = self.jump_mode.take();
            if matches_jump_cancel(data) {
                self.jump_mode = jump;
                return;
            }
            if let Some(printable) = decode_printable(data) {
                if let Some(dir) = jump {
                    self.jump_to_char(&printable, dir);
                }
                return;
            }
        }

        // Bracketed paste can share a raw read with ordinary text before or
        // after the markers. Keep the protocol framing intact and feed each
        // completed paste atomically, including when the end marker arrives
        // in a later read.
        if self.is_in_paste || data.contains("\x1b[200~") {
            self.handle_paste_stream(data);
            return;
        }

        // Kitty flag-2 release events are notifications, not text or editor
        // commands. Keep repeats as ordinary key input. This runs after paste
        // buffering so a chunk containing only printable text cannot bypass a
        // bracketed paste that started in an earlier read.
        if is_key_release(data) {
            return;
        }

        // The upstream editor accepts coalesced printable terminal input at
        // this boundary. Keep named key strings on the command path, while
        // forwarding a safe printable batch through the grapheme-aware burst
        // path so no character is lost when a caller does not pre-classify
        // terminal input.
        if is_printable_input_batch(data, &parsed_key)
            || is_coalesced_controlled_input(data, &parsed_key)
        {
            self.handle_input_burst(data);
            return;
        }

        // Ctrl+C -> copy (handled by the parent loop). Editor ignores.
        if matches_binding("tui.input.copy") {
            return;
        }

        // Undo.
        if matches_binding("tui.editor.undo") {
            self.undo();
            return;
        }

        // Autocomplete mode.
        if self.autocomplete_state.is_some() && self.autocomplete_list.is_some() {
            if matches_binding("tui.select.cancel") {
                self.cancel_autocomplete();
                return;
            }
            if matches_binding("tui.select.up") || matches_binding("tui.select.down") {
                let key = TuiKey::parse_simple(data);
                if let Some(list) = &mut self.autocomplete_list {
                    list.handle_input(&key);
                }
                return;
            }
            if matches_binding("tui.input.tab") {
                let selected = self
                    .autocomplete_list
                    .as_ref()
                    .and_then(|l| l.get_selected_item())
                    .cloned();
                if let Some(selected) = selected {
                    self.push_undo_snapshot();
                    self.last_action = None;
                    self.apply_autocomplete_item(&selected);
                }
                return;
            }
            if matches_binding("tui.select.confirm") {
                let selected = self
                    .autocomplete_list
                    .as_ref()
                    .and_then(|l| l.get_selected_item())
                    .cloned();
                if let Some(selected) = selected {
                    self.push_undo_snapshot();
                    self.last_action = None;
                    let slash = self.autocomplete_prefix.starts_with('/');
                    self.apply_autocomplete_item(&selected);
                    // Close the popup so the next Enter (or the fall-through
                    // below for slash commands) actually submits the line
                    // instead of re-applying the same completion.
                    if let Some(provider) = self.autocomplete_provider.take() {
                        self.cancel_autocomplete();
                        self.autocomplete_provider = Some(provider);
                    }
                    if !slash {
                        return;
                    }
                    // For slash-command completions, fall through to submit.
                }
            }
        }

        // Tab triggers completion.
        if matches_binding("tui.input.tab") && self.autocomplete_state.is_none() {
            self.handle_tab_completion();
            return;
        }

        // Deletion actions.
        if matches_binding("tui.editor.deleteToLineEnd") {
            self.delete_to_end_of_line();
            return;
        }
        if matches_binding("tui.editor.deleteToLineStart") {
            self.delete_to_start_of_line();
            return;
        }
        if matches_binding("tui.editor.deleteWordBackward") {
            self.delete_word_backwards();
            return;
        }
        if matches_binding("tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return;
        }
        if matches_binding("tui.editor.deleteCharBackward") || matches_key(data, "shift+backspace")
        {
            self.handle_backspace();
            return;
        }
        if matches_binding("tui.editor.deleteCharForward") || matches_key(data, "shift+delete") {
            self.handle_forward_delete();
            return;
        }

        // Kill ring.
        if matches_binding("tui.editor.yank") {
            self.yank();
            return;
        }
        if matches_binding("tui.editor.yankPop") {
            self.yank_pop();
            return;
        }

        // Dedicated history actions always browse entries instead of moving
        // the cursor. These are intentionally unbound by default, but the
        // upstream editor supports assigning them independently from the
        // arrow-key cursor actions (for example Ctrl+P/Ctrl+N).
        if matches_binding("tui.editor.historyPrevious") {
            self.cancel_autocomplete();
            self.navigate_history(-1);
            return;
        }
        if matches_binding("tui.editor.historyNext") {
            self.cancel_autocomplete();
            self.navigate_history(1);
            return;
        }

        // Line start/end.
        if matches_binding("tui.editor.cursorLineStart") {
            self.move_to_line_start();
            return;
        }
        if matches_binding("tui.editor.cursorLineEnd") {
            self.move_to_line_end();
            return;
        }
        if matches_binding("tui.editor.cursorWordLeft") {
            self.move_word_backwards();
            return;
        }
        if matches_binding("tui.editor.cursorWordRight") {
            self.move_word_forwards();
            return;
        }

        // New line.
        // Keep the legacy terminal encodings accepted by upstream Pi. In
        // particular, a raw LF is Shift+Enter on terminals that do not emit
        // a distinct modified sequence; it must not fall through to submit.
        let legacy_newline = data == "\n"
            || (data.starts_with('\n') && data.len() > 1)
            || data == "\x1b\r"
            || data == "\x1b[13;2~"
            || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'));
        if matches_binding("tui.input.newLine") || legacy_newline {
            if self.should_submit_on_backslash_enter(data, &bindings) {
                self.handle_backspace();
                self.submit_value();
                return;
            }
            self.add_new_line();
            return;
        }

        // Submit.
        if matches_binding("tui.input.submit") {
            if self.disable_submit {
                return;
            }
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let cursor_col = floor_grapheme_boundary(&current_line, self.state.cursor_col);
            if cursor_col > 0 && current_line[..cursor_col].ends_with('\\') {
                self.handle_backspace();
                self.add_new_line();
                return;
            }
            self.submit_value();
            return;
        }

        // Arrow keys with history support.
        if matches_binding("tui.editor.cursorUp") {
            if self.is_on_first_visual_line()
                && (self.is_editor_empty() || self.history_index > -1 || self.state.cursor_col == 0)
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
            return;
        }
        if matches_binding("tui.editor.cursorDown") {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if matches_binding("tui.editor.cursorRight") {
            self.move_cursor(0, 1);
            return;
        }
        if matches_binding("tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
            return;
        }

        // Page up/down.
        if matches_binding("tui.editor.pageUp") {
            self.page_scroll(-1);
            return;
        }
        if matches_binding("tui.editor.pageDown") {
            self.page_scroll(1);
            return;
        }

        // Character jump mode triggers.
        if matches_binding("tui.editor.jumpForward") {
            self.jump_mode = Some("forward");
            return;
        }
        if matches_binding("tui.editor.jumpBackward") {
            self.jump_mode = Some("backward");
            return;
        }

        // Shift+Space -> space.
        if matches_key(data, "shift+space") {
            self.insert_character(" ");
            return;
        }

        if let Some(printable) = decode_printable(data) {
            self.insert_character(&printable);
            return;
        }
        // Raw printable characters.
        if let Some(c) = data.chars().next() {
            if (c as u32) >= 32 {
                self.insert_character(&c.to_string());
            }
        }
    }

    fn handle_paste_stream(&mut self, data: &str) {
        const START: &str = "\x1b[200~";
        const END: &str = "\x1b[201~";
        let mut pending = data.to_string();

        loop {
            if !self.is_in_paste {
                let Some(start) = pending.find(START) else {
                    if !pending.is_empty() {
                        self.handle_input(&pending);
                    }
                    return;
                };

                if start > 0 {
                    let prefix = pending[..start].to_string();
                    self.handle_input(&prefix);
                }
                self.is_in_paste = true;
                self.paste_buffer.clear();
                pending = pending[start + START.len()..].to_string();
            }

            // Search the accumulated paste buffer, not only the latest read.
            // Terminal input can split the six-byte end marker between reads
            // (for example `ESC[20` followed by `1~`).
            self.paste_buffer.push_str(&pending);
            let Some(end) = self.paste_buffer.find(END) else {
                return;
            };

            let remaining = self.paste_buffer[end + END.len()..].to_string();
            self.paste_buffer.truncate(end);
            let paste_content = std::mem::take(&mut self.paste_buffer);
            if !paste_content.is_empty() {
                self.handle_paste(&paste_content);
            }
            self.is_in_paste = false;
            pending = remaining;
            if pending.is_empty() {
                return;
            }
        }
    }

    // ------------------------------------------------------------------ editing

    fn should_submit_on_backslash_enter(
        &self,
        data: &str,
        bindings: &crate::keybindings::KeybindingsManager,
    ) -> bool {
        if self.disable_submit || !bindings.matches_raw(data, "enter") {
            return false;
        }
        let has_shift_enter = bindings
            .get_keys("tui.input.submit")
            .iter()
            .any(|key| key == "shift+enter" || key == "shift+return");
        if !has_shift_enter {
            return false;
        }
        let Some(line) = self.state.lines.get(self.state.cursor_line) else {
            return false;
        };
        let cursor = floor_grapheme_boundary(line, self.state.cursor_col);
        cursor > 0 && line[..cursor].ends_with('\\')
    }

    fn insert_character(&mut self, char: &str) {
        self.exit_history_browsing();

        let is_ws = char
            .chars()
            .next()
            .map(|c| c.is_whitespace())
            .unwrap_or(false);
        if is_ws || self.last_action != Some("type-word") {
            self.push_undo_snapshot();
        }
        self.last_action = Some("type-word");

        let line = self
            .state
            .lines
            .get_mut(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let cursor_col = floor_grapheme_boundary(&line, self.state.cursor_col);
        let before = line[..cursor_col].to_string();
        let after = line[cursor_col..].to_string();
        self.state.lines[self.state.cursor_line] = format!("{before}{char}{after}");
        self.set_cursor_col(cursor_col + char.len());

        // Autocomplete triggers.
        if self.autocomplete_state.is_none() {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let cursor_col = floor_grapheme_boundary(&current_line, self.state.cursor_col);
            let text_before_cursor = current_line[..cursor_col].to_string();
            if char == "/" && self.is_at_start_of_message() {
                self.try_trigger_autocomplete(false);
            } else if self
                .autocomplete_trigger_characters
                .iter()
                .any(|t| t == char)
            {
                let chars: Vec<char> = text_before_cursor.chars().collect();
                let char_before_symbol = if chars.len() >= 2 {
                    Some(chars[chars.len() - 2])
                } else {
                    None
                };
                if text_before_cursor.chars().count() == 1
                    || char_before_symbol == Some(' ')
                    || char_before_symbol == Some('\t')
                {
                    self.try_trigger_autocomplete(false);
                }
            } else if char
                .chars()
                .next()
                .map(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                .unwrap_or(false)
                && (self.is_in_slash_command_context(&text_before_cursor)
                    || self.autocomplete_trigger_pattern(&text_before_cursor))
            {
                self.try_trigger_autocomplete(false);
            }
        } else {
            self.update_autocomplete();
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn autocomplete_trigger_pattern(&self, text_before_cursor: &str) -> bool {
        // (?:^|[\s])[@#%][^\s]*$  — symbols at token boundaries.
        let last_delim = text_before_cursor
            .rfind([' ', '\t'])
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &text_before_cursor[last_delim..];
        if token.is_empty() {
            return false;
        }
        let first = token.chars().next().unwrap();
        self.autocomplete_trigger_characters
            .iter()
            .any(|t| t == &first.to_string())
    }

    fn handle_paste(&mut self, pasted_text: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo_snapshot();

        // Decode CSI-u Ctrl+<letter> sequences back to control bytes.
        let decoded_text = decode_paste_control_bytes(pasted_text);

        let clean_text = self.normalize_text(&decoded_text);

        // Filter non-printable except newlines.
        let filtered: String = clean_text
            .chars()
            .filter(|c| *c == '\n' || (*c as u32) >= 32)
            .collect();

        let mut filtered_text = filtered;
        if filtered_text.starts_with('/')
            || filtered_text.starts_with('~')
            || filtered_text.starts_with('.')
        {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let char_before = if self.state.cursor_col > 0 {
                let cursor_col = floor_grapheme_boundary(&current_line, self.state.cursor_col);
                current_line[..cursor_col]
                    .chars()
                    .next_back()
                    .unwrap_or_default()
            } else {
                ' '
            };
            if char_before.is_alphanumeric() || char_before == '_' {
                filtered_text = format!(" {filtered_text}");
            }
        }

        let pasted_line_count = filtered_text.split('\n').count();
        let total_chars = filtered_text.chars().count();
        if pasted_line_count > 10 || total_chars > 1000 {
            self.paste_counter += 1;
            let paste_id = self.paste_counter;
            self.pastes.insert(paste_id, filtered_text);
            let marker = if pasted_line_count > 10 {
                format!("[paste #{paste_id} +{pasted_line_count} lines]")
            } else {
                format!("[paste #{paste_id} {total_chars} chars]")
            };
            self.insert_text_at_cursor_internal(&marker);
            return;
        }

        self.insert_text_at_cursor_internal(&filtered_text);
    }

    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo_snapshot();

        let current_line = self.state.lines[self.state.cursor_line].clone();
        let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();

        self.state.lines[self.state.cursor_line] = before;
        self.state.lines.insert(self.state.cursor_line + 1, after);
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
    }

    fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.state.lines.join("\n"))
            .trim()
            .to_string();

        self.state = EditorState::empty();
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.undo_stack.clear();
        self.last_action = None;
        self.submit_pending = Some(result);
    }

    /// Drain a pending submitted prompt (None when none pending).
    pub fn drain_submitted(&mut self) -> Option<String> {
        self.submit_pending.take()
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn handle_backspace(&mut self) {
        self.cancel_autocomplete_request();
        self.exit_history_browsing();
        self.last_action = None;

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();

            let mut line = self.state.lines[self.state.cursor_line].clone();
            let before_cursor = line[..self.state.cursor_col.min(line.len())].to_string();

            let graphemes = self.segment(&before_cursor, "grapheme");
            let last_grapheme = graphemes.last().cloned();
            let grapheme_len = last_grapheme.as_ref().map(|g| g.segment.len()).unwrap_or(1);
            let mut removed_paste_marker = false;

            // Paste-marker handling: backspace removes the marker + registry.
            if let Some(g) = &last_grapheme {
                if is_paste_marker(&g.segment) {
                    if let Some(cap) = paste_marker_regex().captures(&g.segment) {
                        let target_id: usize = cap.get(1).unwrap().as_str().parse().unwrap_or(0);
                        let marker_start = g.index.min(line.len());
                        let marker_end =
                            marker_start.saturating_add(g.segment.len()).min(line.len());
                        let before_marker = line[..marker_start].to_string();
                        let after_marker = line[marker_end..].to_string();

                        self.pastes.remove(&target_id);
                        self.paste_counter = self.paste_counter.saturating_sub(1);
                        // Renumber higher ids down by one.
                        let mut higher: Vec<usize> = self
                            .pastes
                            .keys()
                            .copied()
                            .filter(|id| *id > target_id)
                            .collect();
                        higher.sort();
                        for id in higher {
                            if let Some(content) = self.pastes.remove(&id) {
                                self.pastes.insert(id - 1, content);
                            }
                        }

                        // Remove exactly the atomic marker before renumbering
                        // the remaining text. Renumbering first can change a
                        // marker's byte length (for example #10 -> #9), so
                        // reusing the old cursor/length would delete part of
                        // the text that followed the marker.
                        let renumber = |text: &str| {
                            paste_marker_regex()
                                .replace_all(text, |caps: &regex::Captures| {
                                    let x: usize =
                                        caps.get(1).unwrap().as_str().parse().unwrap_or(0);
                                    if x <= target_id {
                                        return caps.get(0).unwrap().as_str().to_string();
                                    }
                                    let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                                    format!("[paste #{}{suffix}]", x - 1)
                                })
                                .to_string()
                        };
                        let mut renumbered: Vec<String> =
                            Vec::with_capacity(self.state.lines.len());
                        for (line_index, current) in self.state.lines.iter().enumerate() {
                            if line_index == self.state.cursor_line {
                                renumbered.push(format!(
                                    "{}{}",
                                    renumber(&before_marker),
                                    renumber(&after_marker)
                                ));
                            } else {
                                renumbered.push(renumber(current));
                            }
                        }
                        let new_cursor = renumber(&before_marker).len();
                        self.state.lines = renumbered;
                        self.set_cursor_col(new_cursor);
                        removed_paste_marker = true;
                    }
                }
            }

            if !removed_paste_marker {
                line = self.state.lines[self.state.cursor_line].clone();
                let cursor_col = self.state.cursor_col;
                let before =
                    line[..cursor_col.saturating_sub(grapheme_len).min(line.len())].to_string();
                let after = line[cursor_col.min(line.len())..].to_string();
                self.state.lines[self.state.cursor_line] = format!("{before}{after}");
                self.set_cursor_col(self.state.cursor_col.saturating_sub(grapheme_len));
            }
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.len());
        }

        // Update autocomplete after backspace.
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        } else {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let text_before_cursor =
                current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            if self.is_in_slash_command_context(&text_before_cursor)
                || self.autocomplete_trigger_pattern(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn set_cursor_col(&mut self, col: usize) {
        let normalized = self
            .state
            .lines
            .get(self.state.cursor_line)
            .map(|line| floor_grapheme_boundary(line, col))
            .unwrap_or(0);
        self.state.cursor_col = normalized;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    fn set_cursor_col_grapheme(&mut self, col: usize) {
        let normalized = self
            .state
            .lines
            .get(self.state.cursor_line)
            .map(|line| floor_grapheme_boundary(line, col))
            .unwrap_or(0);
        self.set_cursor_col(normalized);
    }
}

impl Editor {
    // ------------------------------------------------------------------ deletion & lines

    fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let deleted_text =
                current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            self.kill_ring.push(
                &deleted_text,
                KillRingPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some("kill"),
                },
            );
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] =
                current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                KillRingPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some("kill"),
                },
            );
            self.last_action = Some("kill");
            let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.len());
        }
    }

    fn delete_to_end_of_line(&mut self) {
        self.exit_history_browsing();
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let deleted_text =
                current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.kill_ring.push(
                &deleted_text,
                KillRingPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some("kill"),
                },
            );
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] =
                current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                KillRingPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some("kill"),
                },
            );
            self.last_action = Some("kill");
            let next_line = self.state.lines[self.state.cursor_line + 1].clone();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }
    }

    fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    KillRingPushOptions {
                        prepend: true,
                        accumulate: self.last_action == Some("kill"),
                    },
                );
                self.last_action = Some("kill");
                let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
                self.state.lines[self.state.cursor_line - 1] =
                    format!("{previous_line}{current_line}");
                self.state.lines.remove(self.state.cursor_line);
                self.state.cursor_line -= 1;
                self.set_cursor_col(previous_line.len());
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action == Some("kill");
            let old_cursor_col = self.state.cursor_col;
            self.move_word_backwards();
            let delete_from = self.state.cursor_col;
            self.set_cursor_col(old_cursor_col);
            let deleted_text = current_line[delete_from.min(current_line.len())
                ..self.state.cursor_col.min(current_line.len())]
                .to_string();
            self.kill_ring.push(
                &deleted_text,
                KillRingPushOptions {
                    prepend: true,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] = format!(
                "{}{}",
                &current_line[..delete_from.min(current_line.len())],
                &current_line[self.state.cursor_col.min(current_line.len())..]
            );
            self.set_cursor_col(delete_from);
        }
    }

    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col >= current_line.len() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    KillRingPushOptions {
                        prepend: false,
                        accumulate: self.last_action == Some("kill"),
                    },
                );
                self.last_action = Some("kill");
                let next_line = self.state.lines[self.state.cursor_line + 1].clone();
                self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
                self.state.lines.remove(self.state.cursor_line + 1);
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action == Some("kill");
            let old_cursor_col = self.state.cursor_col;
            self.move_word_forwards();
            let delete_to = self.state.cursor_col;
            self.set_cursor_col(old_cursor_col);
            let deleted_text = current_line
                [self.state.cursor_col.min(current_line.len())..delete_to.min(current_line.len())]
                .to_string();
            self.kill_ring.push(
                &deleted_text,
                KillRingPushOptions {
                    prepend: false,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] = format!(
                "{}{}",
                &current_line[..self.state.cursor_col.min(current_line.len())],
                &current_line[delete_to.min(current_line.len())..]
            );
        }
    }

    fn handle_forward_delete(&mut self) {
        self.cancel_autocomplete_request();
        self.exit_history_browsing();
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let after_cursor = &current_line[self.state.cursor_col.min(current_line.len())..];
            let graphemes = self.segment(after_cursor, "grapheme");
            let first_grapheme = graphemes.first().cloned();
            let grapheme_len = first_grapheme.map(|g| g.segment.len()).unwrap_or(1);
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line
                [(self.state.cursor_col + grapheme_len).min(current_line.len())..]
                .to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();
            let next_line = self.state.lines[self.state.cursor_line + 1].clone();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }

        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        } else {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let text_before_cursor =
                current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            if self.is_in_slash_command_context(&text_before_cursor)
                || self.autocomplete_trigger_pattern(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn move_to_line_start(&mut self) {
        self.cancel_autocomplete_request();
        self.last_action = None;
        self.set_cursor_col(0);
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
    }

    fn move_to_line_end(&mut self) {
        self.cancel_autocomplete_request();
        self.last_action = None;
        let len = self
            .state
            .lines
            .get(self.state.cursor_line)
            .map(|l| l.len())
            .unwrap_or(0);
        self.set_cursor_col(len);
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
    }

    // ------------------------------------------------------------------ vertical movement

    fn build_visual_line_map(&self, width: usize) -> Vec<(usize, usize, usize)> {
        let mut visual_lines: Vec<(usize, usize, usize)> = Vec::new(); // (logicalLine, startCol, length)
        for (i, line) in self.state.lines.iter().enumerate() {
            let line_vis_width = visible_width(line);
            if line.is_empty() {
                visual_lines.push((i, 0, 0));
            } else if line_vis_width <= width {
                visual_lines.push((i, 0, line.len()));
            } else {
                let segmented = self.segment(line, "grapheme");
                let chunks = word_wrap_line(line, width, Some(&segmented));
                for chunk in chunks {
                    visual_lines.push((i, chunk.start_index, chunk.end_index - chunk.start_index));
                }
            }
        }
        visual_lines
    }

    fn find_visual_line_at(
        &self,
        visual_lines: &[(usize, usize, usize)],
        line: usize,
        col: usize,
    ) -> usize {
        let last = visual_lines.len().saturating_sub(1);
        for (i, vl) in visual_lines.iter().enumerate() {
            if vl.0 != line {
                continue;
            }
            let offset = col as isize - vl.1 as isize;
            let is_last_segment = i == visual_lines.len() - 1 || visual_lines[i + 1].0 != vl.0;
            if offset >= 0
                && (offset < vl.2 as isize || (is_last_segment && offset == vl.2 as isize))
            {
                return i;
            }
        }
        last
    }

    fn find_current_visual_line(&self, visual_lines: &[(usize, usize, usize)]) -> usize {
        self.find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    fn move_to_visual_line(
        &mut self,
        visual_lines: &[(usize, usize, usize)],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(&(tgt_line, tgt_start, tgt_end)) = visual_lines.get(target_visual_line) else {
            return;
        };
        let (cur_line, cur_start, cur_end) = visual_lines[current_visual_line];
        let source_text = self.state.lines.get(cur_line).cloned().unwrap_or_default();

        let current_visual_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let vl_index = self.find_visual_line_at(visual_lines, cur_line, snapped);
                let snapped_start = visual_lines[vl_index].1;
                visual_column_between(&source_text, snapped_start, snapped)
            }
            None => visual_column_between(&source_text, cur_start, self.state.cursor_col),
        };

        let is_last_source = current_visual_line == visual_lines.len() - 1
            || visual_lines[current_visual_line + 1].0 != cur_line;
        let source_width = visual_column_between(&source_text, cur_start, cur_end);
        let source_max = if is_last_source {
            source_width
        } else {
            source_width.saturating_sub(1)
        };
        let is_last_target = target_visual_line == visual_lines.len() - 1
            || visual_lines
                .get(target_visual_line + 1)
                .map(|v| v.0)
                .unwrap_or(tgt_line + 1)
                != tgt_line;
        let logical_line = self.state.lines.get(tgt_line).cloned().unwrap_or_default();
        let target_width = visual_column_between(&logical_line, tgt_start, tgt_end);
        let target_max = if is_last_target {
            target_width
        } else {
            target_width.saturating_sub(1)
        };

        let move_to_col =
            self.compute_vertical_move_column(current_visual_col, source_max, target_max);

        self.state.cursor_line = tgt_line;
        let target_col = byte_offset_for_visual_column(&logical_line, tgt_start, move_to_col);
        self.set_cursor_col(target_col.min(tgt_end));

        // Snap to atomic segment boundary (paste markers).
        let segments = self.segment(&logical_line, "grapheme");
        for seg in &segments {
            if seg.index > self.state.cursor_col {
                break;
            }
            if seg.segment.len() <= 1 {
                continue;
            }
            if self.state.cursor_col < seg.index + seg.segment.len() {
                let is_continuation = seg.index < tgt_start;
                let is_moving_down = target_visual_line > current_visual_line;
                if is_continuation && is_moving_down {
                    let seg_end = seg.index + seg.segment.len();
                    let mut next = target_visual_line + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].0 == tgt_line
                        && visual_lines[next].1 < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current_visual_line, next);
                        return;
                    }
                }
                let snapped = self.state.cursor_col;
                self.set_cursor_col(seg.index);
                self.snapped_from_cursor_col = Some(snapped);
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max: usize,
        target_max: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max;
        let target_too_short = target_max < current_visual_col;

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                self.preferred_visual_col = Some(current_visual_col);
                return target_max;
            }
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let preferred = self.preferred_visual_col.unwrap_or(current_visual_col);
        let target_cant_fit = target_max < preferred;
        if target_too_short || target_cant_fit {
            return target_max;
        }
        self.preferred_visual_col = None;
        preferred
    }

    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.cancel_autocomplete_request();
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width.load(Ordering::Relaxed));
        let current_visual_line = self.find_current_visual_line(&visual_lines);

        if delta_line != 0 {
            let target = current_visual_line as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_visual_line, target as usize);
            }
        }

        if delta_col != 0 {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            if delta_col > 0 {
                if self.state.cursor_col < current_line.len() {
                    let after_cursor =
                        &current_line[self.state.cursor_col.min(current_line.len())..];
                    let graphemes = self.segment(after_cursor, "grapheme");
                    let first = graphemes.first().cloned();
                    self.set_cursor_col(
                        self.state.cursor_col + first.map(|g| g.segment.len()).unwrap_or(1),
                    );
                } else if self.state.cursor_line < self.state.lines.len() - 1 {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else {
                    if let Some(vl) = visual_lines.get(current_visual_line) {
                        self.preferred_visual_col =
                            Some(self.state.cursor_col.saturating_sub(vl.1));
                    }
                }
            } else {
                if self.state.cursor_col > 0 {
                    let before_cursor =
                        &current_line[..self.state.cursor_col.min(current_line.len())];
                    let graphemes = self.segment(before_cursor, "grapheme");
                    let last = graphemes.last().cloned();
                    self.set_cursor_col(
                        self.state
                            .cursor_col
                            .saturating_sub(last.map(|g| g.segment.len()).unwrap_or(1)),
                    );
                } else if self.state.cursor_line > 0 {
                    self.state.cursor_line -= 1;
                    let prev_len = self
                        .state
                        .lines
                        .get(self.state.cursor_line)
                        .map(|l| l.len())
                        .unwrap_or(0);
                    self.set_cursor_col(prev_len);
                }
            }
        }

        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
    }

    fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page_size = std::cmp::max(5, self.terminal_rows * 3 / 10);
        let visual_lines = self.build_visual_line_map(self.last_width.load(Ordering::Relaxed));
        let current = self.find_current_visual_line(&visual_lines);
        let target = (current as isize + direction * page_size as isize)
            .clamp(0, visual_lines.len().saturating_sub(1) as isize) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    // ------------------------------------------------------------------ word navigation

    fn move_word_backwards(&mut self) {
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let prev_len = self
                    .state
                    .lines
                    .get(self.state.cursor_line)
                    .map(|l| l.len())
                    .unwrap_or(0);
                self.set_cursor_col(prev_len);
            }
            return;
        }

        let cursor = self.state.cursor_col.min(current_line.len());
        let opts = WordNavigationOptions {
            segment: Some(&|text: &str| self.segment(text, "word")),
            is_atomic_segment: Some(&is_paste_marker),
        };
        let pos = find_word_backward(&current_line, cursor, &opts);
        self.set_cursor_col(pos);
    }

    fn move_word_forwards(&mut self) {
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col >= current_line.len() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }

        let cursor = self.state.cursor_col.min(current_line.len());
        let opts = WordNavigationOptions {
            segment: Some(&|text: &str| self.segment(text, "word")),
            is_atomic_segment: Some(&is_paste_marker),
        };
        let pos = find_word_forward(&current_line, cursor, &opts);
        self.set_cursor_col(pos);
    }

    // ------------------------------------------------------------------ kill/yank

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.push_undo_snapshot();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some("yank");
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some("yank") || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some("yank");
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        let lines: Vec<&str> = text.split('\n').collect();
        if lines.len() == 1 {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{text}{after}");
            self.set_cursor_col(self.state.cursor_col + text.len());
        } else {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{}", lines[0]);
            for (idx, line) in lines
                .iter()
                .skip(1)
                .take(lines.len().saturating_sub(2))
                .enumerate()
            {
                self.state
                    .lines
                    .insert(self.state.cursor_line + idx + 1, line.to_string());
            }
            let last_line_index = self.state.cursor_line + lines.len() - 1;
            self.state.lines.insert(
                last_line_index,
                format!("{}{after}", lines[lines.len() - 1]),
            );
            self.state.cursor_line = last_line_index;
            self.set_cursor_col(lines[lines.len() - 1].len());
        }
    }

    fn delete_yanked_text(&mut self) {
        let yanked_text = self.kill_ring.peek().map(|s| s.to_string());
        let Some(yanked_text) = yanked_text else {
            return;
        };
        let yank_lines: Vec<&str> = yanked_text.split('\n').collect();

        if yank_lines.len() == 1 {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let delete_len = yanked_text.len();
            let before = current_line[..self
                .state
                .cursor_col
                .saturating_sub(delete_len)
                .min(current_line.len())]
                .to_string();
            let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col.saturating_sub(delete_len));
        } else {
            let start_line = self.state.cursor_line.saturating_sub(yank_lines.len() - 1);
            let first_len = yank_lines[0].len();
            let start_col = self
                .state
                .lines
                .get(start_line)
                .map(|l| l.len().saturating_sub(first_len))
                .unwrap_or(0);
            let after_cursor = self
                .state
                .lines
                .get(self.state.cursor_line)
                .map(|l| l[self.state.cursor_col.min(l.len())..].to_string())
                .unwrap_or_default();
            let before_yank = self
                .state
                .lines
                .get(start_line)
                .map(|l| l[..start_col.min(l.len())].to_string())
                .unwrap_or_default();
            // Replace the yanked span (startLine..=cursorLine) with the
            // merged before-yank + after-cursor line.
            self.state.lines.drain(start_line..=self.state.cursor_line);
            self.state
                .lines
                .insert(start_line, format!("{before_yank}{after_cursor}"));
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
    }

    // ------------------------------------------------------------------ undo

    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
    }

    // ------------------------------------------------------------------ jump

    fn jump_to_char(&mut self, ch: &str, direction: &str) {
        self.last_action = None;
        let is_forward = direction == "forward";
        let lines = self.state.lines.clone();
        let end: isize = if is_forward { lines.len() as isize } else { -1 };
        let step: isize = if is_forward { 1 } else { -1 };
        let mut line_idx = self.state.cursor_line as isize;
        while line_idx != end {
            let line = &lines[line_idx as usize];
            let is_current = line_idx as usize == self.state.cursor_line;
            let found = if is_forward {
                let search_from = if is_current {
                    next_grapheme_boundary(
                        line,
                        floor_grapheme_boundary(line, self.state.cursor_col),
                    )
                } else {
                    0
                };
                line[search_from..].find(ch).map(|i| search_from + i)
            } else {
                let search_from = if is_current {
                    floor_grapheme_boundary(line, self.state.cursor_col)
                } else {
                    line.len()
                };
                line[..search_from].rfind(ch)
            };
            if let Some(idx) = found {
                self.state.cursor_line = line_idx as usize;
                self.set_cursor_col(idx);
                return;
            }
            line_idx += step;
        }
    }

    // ------------------------------------------------------------------ slash/autocomplete context

    fn is_slash_menu_allowed(&self) -> bool {
        self.state.cursor_line == 0
    }

    fn is_at_start_of_message(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        let t = before_cursor.trim();
        t.is_empty() || t == "/"
    }

    fn is_in_slash_command_context(&self, text_before_cursor: &str) -> bool {
        self.is_slash_menu_allowed() && text_before_cursor.trim_start().starts_with('/')
    }
}

impl Editor {
    // ------------------------------------------------------------------ autocomplete

    fn set_autocomplete_trigger_characters(&mut self, trigger_characters: Vec<String>) {
        let mut next = vec!["@".to_string(), "#".to_string()];
        for character in trigger_characters {
            let is_single = character.chars().count() == 1;
            let c = character.chars().next();
            let is_space = c.map(|c| c.is_whitespace()).unwrap_or(false);
            if !is_single || character == "/" || is_space || next.contains(&character) {
                continue;
            }
            next.push(character);
        }
        self.autocomplete_trigger_characters = next;
    }

    fn get_best_autocomplete_match_index(items: &[(String, String)], prefix: &str) -> isize {
        if prefix.is_empty() {
            return -1;
        }
        let mut first_prefix_index = -1isize;
        for (i, (value, _)) in items.iter().enumerate() {
            if value == prefix {
                return i as isize;
            }
            if first_prefix_index == -1 && value.starts_with(prefix) {
                first_prefix_index = i as isize;
            }
        }
        first_prefix_index
    }

    fn create_autocomplete_list(&self, prefix: &str, items: Vec<AutocompleteItem>) -> SelectList {
        let layout = if prefix.starts_with('/') {
            Some(SLASH_COMMAND_SELECT_LIST_LAYOUT)
        } else {
            None
        };
        let select_items: Vec<SelectItem> = items
            .into_iter()
            .map(|i| SelectItem::new(i.value, i.label, i.description))
            .collect();
        let mut list = SelectList::new(
            select_items,
            self.autocomplete_max_visible,
            crate::components::select_list::plain_theme(),
            layout.unwrap_or_default(),
        );
        let match_index = Self::get_best_autocomplete_match_index(
            &list
                .items()
                .iter()
                .map(|i| (i.value.clone(), i.label.clone()))
                .collect::<Vec<_>>(),
            prefix,
        );
        if match_index >= 0 {
            list.set_selected_index(match_index as usize);
        }
        list
    }

    fn try_trigger_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(false, explicit_tab);
    }

    fn handle_tab_completion(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        if self.is_in_slash_command_context(&before_cursor)
            && !before_cursor.trim_start().contains(' ')
        {
            self.request_autocomplete(false, true);
        } else {
            self.force_file_autocomplete(true);
        }
    }

    fn force_file_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(true, explicit_tab);
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        self.cancel_autocomplete_request();

        // Attachment completion is intentionally delayed so a burst of
        // `@foo`/`#123` input produces one provider call. Slash command and
        // explicit Tab completion remain immediate, matching upstream.
        if !force && !explicit_tab && self.should_debounce_attachment_completion() {
            let generation = self.autocomplete_generation;
            self.autocomplete_pending = Some(PendingAutocomplete {
                force,
                explicit_tab,
                generation,
                due: Instant::now() + AUTOCOMPLETE_DEBOUNCE,
            });
            return;
        }

        let generation = self.autocomplete_generation;
        self.request_autocomplete_now(force, explicit_tab, generation);
    }

    fn should_debounce_attachment_completion(&self) -> bool {
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let text_before_cursor =
            current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        !self.is_in_slash_command_context(&text_before_cursor)
            && self.autocomplete_trigger_pattern(&text_before_cursor)
    }

    fn request_autocomplete_now(&mut self, force: bool, explicit_tab: bool, generation: u64) {
        let Some(provider) = self.autocomplete_provider.take() else {
            return;
        };

        let should_proceed = !force
            || provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
            );
        if !should_proceed {
            self.autocomplete_provider = Some(provider);
            return;
        }

        let request_lines = self.state.lines.clone();
        let request_cursor_line = self.state.cursor_line;
        let request_cursor_col = self.state.cursor_col;
        let aborted = Arc::new(AtomicBool::new(false));
        self.autocomplete_abort = Some(aborted.clone());
        let suggestions = provider.get_suggestions(
            &request_lines,
            request_cursor_line,
            request_cursor_col,
            force,
            aborted.as_ref(),
        );
        let request_is_current = self.autocomplete_generation == generation
            && !aborted.load(Ordering::SeqCst)
            && self.state.lines == request_lines
            && self.state.cursor_line == request_cursor_line
            && self.state.cursor_col == request_cursor_col;
        self.autocomplete_abort = None;

        if !request_is_current {
            self.autocomplete_provider = Some(provider);
            return;
        }

        match suggestions {
            None => {
                self.autocomplete_provider = Some(provider);
                self.clear_autocomplete_ui();
            }
            Some(suggestions) => {
                if suggestions.items.is_empty() {
                    self.autocomplete_provider = Some(provider);
                    self.clear_autocomplete_ui();
                    return;
                }
                if force && explicit_tab && suggestions.items.len() == 1 {
                    let item = suggestions.items[0].clone();
                    self.push_undo_snapshot();
                    self.last_action = None;
                    self.apply_completion_result(provider.as_ref(), &item, &suggestions.prefix);
                    self.autocomplete_provider = Some(provider);
                    return;
                }
                self.autocomplete_provider = Some(provider);
                self.apply_autocomplete_suggestions(
                    suggestions,
                    if force { "force" } else { "regular" },
                );
            }
        }
    }

    fn apply_autocomplete_suggestions(
        &mut self,
        suggestions: AutocompleteSuggestions,
        state: &'static str,
    ) {
        self.autocomplete_prefix = suggestions.prefix.clone();
        let mut list = self.create_autocomplete_list(&suggestions.prefix, suggestions.items);
        let prefix = suggestions.prefix.clone();
        let best = Self::get_best_autocomplete_match_index(
            &list
                .items()
                .iter()
                .map(|i| (i.value.clone(), i.label.clone()))
                .collect::<Vec<_>>(),
            &prefix,
        );
        if best >= 0 {
            list.set_selected_index(best as usize);
        }
        self.autocomplete_list = Some(list);
        self.autocomplete_state = Some(state);
    }

    fn apply_completion_result(
        &mut self,
        provider: &(dyn AutocompleteProvider + Send + Sync),
        item: &AutocompleteItem,
        prefix: &str,
    ) {
        let result = provider.apply_completion(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            item,
            prefix,
        );
        self.state.lines = if result.lines.is_empty() {
            vec![String::new()]
        } else {
            result.lines
        };
        self.state.cursor_line = result
            .cursor_line
            .min(self.state.lines.len().saturating_sub(1));
        self.set_cursor_col_grapheme(result.cursor_col);
        self.cancel_autocomplete();
    }

    fn apply_autocomplete_item(&mut self, selected: &SelectItem) {
        let Some(provider) = self.autocomplete_provider.take() else {
            return;
        };
        let item = AutocompleteItem {
            value: selected.value.clone(),
            label: selected.label.clone(),
            description: selected.description.clone(),
        };
        let prefix = self.autocomplete_prefix.clone();
        self.apply_completion_result(provider.as_ref(), &item, &prefix);
        self.autocomplete_provider = Some(provider);
    }

    fn update_autocomplete(&mut self) {
        if self.autocomplete_state.is_none() || self.autocomplete_provider.is_none() {
            return;
        }
        let force = self.autocomplete_state == Some("force");
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        if !force
            && !self.is_in_slash_command_context(&before_cursor)
            && !self.autocomplete_trigger_pattern(&before_cursor)
        {
            self.cancel_autocomplete();
            return;
        }
        self.request_autocomplete(force, false);
    }

    fn cancel_autocomplete_request(&mut self) {
        self.autocomplete_generation = self.autocomplete_generation.wrapping_add(1);
        self.autocomplete_pending = None;
        if let Some(aborted) = &self.autocomplete_abort {
            aborted.store(true, Ordering::SeqCst);
        }
        self.autocomplete_abort = None;
    }

    fn clear_autocomplete_ui(&mut self) {
        self.autocomplete_state = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
    }

    fn cancel_autocomplete(&mut self) {
        self.cancel_autocomplete_request();
        self.clear_autocomplete_ui();
    }

    /// Execute a due natural autocomplete request. The interactive event loop
    /// can call this once per frame; it returns whether a request was run.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn drain_autocomplete_tick(&mut self) -> bool {
        let Some(pending) = self.autocomplete_pending.as_ref() else {
            return false;
        };
        if pending.due > Instant::now() {
            return false;
        }
        let pending = self.autocomplete_pending.take().expect("pending request");
        if pending.generation != self.autocomplete_generation {
            return false;
        }
        self.request_autocomplete_now(pending.force, pending.explicit_tab, pending.generation);
        true
    }

    /// Run a pending request immediately. This deterministic hook is useful
    /// for tests and for event loops that need completion before the next
    /// render pass without sleeping for the debounce interval.
    pub fn flush_autocomplete(&mut self) {
        let Some(pending) = self.autocomplete_pending.take() else {
            return;
        };
        if pending.generation == self.autocomplete_generation {
            self.request_autocomplete_now(pending.force, pending.explicit_tab, pending.generation);
        }
    }

    pub fn is_autocomplete_pending(&self) -> bool {
        self.autocomplete_pending.is_some()
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    pub fn current_autocomplete_selection(&self) -> Option<SelectItem> {
        self.autocomplete_list
            .as_ref()
            .and_then(|l| l.get_selected_item().cloned())
    }
}

// ------------------------------------------------------------------ helper fns

/// True when `data` matches a key-string pattern (terminal-normalized surface).
fn matches_key(data: &str, pattern: &str) -> bool {
    matches_raw_key(data, pattern)
}

fn matches_jump_cancel(data: &str) -> bool {
    matches_key(data, "ctrl+]") || matches_key(data, "ctrl+alt+]")
}

fn decode_printable(data: &str) -> Option<String> {
    if let Some((text, _modifier)) = crate::keys::decode_printable_key(data) {
        return Some(text);
    }
    let c = data.chars().next()?;
    if data.len() == c.len_utf8() && (c as u32) >= 32 && !c.is_control() {
        if matches_key(data, "enter")
            || matches_key(data, "tab")
            || matches_key(data, "backspace")
            || matches_key(data, "delete")
            || matches_key(data, "up")
            || matches_key(data, "down")
            || matches_key(data, "left")
            || matches_key(data, "right")
        {
            return None;
        }
        return Some(c.to_string());
    }
    // Single-letter keys typed as "a" are handled above; multi-char names are
    // non-printable control key strings.
    None
}

fn is_printable_input_batch(data: &str, key: &TuiKey) -> bool {
    data.chars().count() > 1
        && !data.contains('\x1b')
        && data.chars().all(|character| !character.is_control())
        && !key.ctrl
        && !key.alt
        && !key.shift
        && !matches!(
            data,
            "enter"
                | "return"
                | "esc"
                | "escape"
                | "backspace"
                | "delete"
                | "tab"
                | "shift+tab"
                | "up"
                | "down"
                | "left"
                | "right"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
                | "pageUp"
                | "pageDown"
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

/// Recognize a raw read that contains ordinary text plus control bytes. The
/// terminal buffer normally emits these as separate events, but callers that
/// provide a larger read must retain the control semantics (for example the
/// newline in `"before\nafter"`) instead of inserting the first scalar only.
fn is_coalesced_controlled_input(data: &str, key: &TuiKey) -> bool {
    data.chars().count() > 1
        && !data.contains('\x1b')
        && data.chars().any(char::is_control)
        && !key.ctrl
        && !key.alt
        && !key.shift
        && !key.super_key
}

/// Decode CSI-u Ctrl+<letter> sequences inside bracketed paste back to bytes.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn decode_paste_control_bytes(text: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[(\d+);5u").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let code: u32 = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
        match code {
            97..=122 => ((code - 96) as u8 as char).to_string(),
            65..=90 => ((code - 64) as u8 as char).to_string(),
            _ => caps.get(0).unwrap().as_str().to_string(),
        }
    })
    .to_string()
}

impl TuiKey {
    /// Parse a canonical key string back into a key.
    pub fn parse_simple(raw: &str) -> TuiKey {
        crate::keys::parse_key(raw)
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        Editor::render_editor(self, width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        // The terminal backend normalizes raw data into key strings; the
        // editor consumes those strings. When called via the Component trait
        // we reconstruct the key-string surface from the parsed key.
        let canonical = key.canonical();
        self.handle_input(&canonical);
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate.
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[path = "editor_tests.rs"]
#[cfg(test)]
mod editor_tests;
