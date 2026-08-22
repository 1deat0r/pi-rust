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
use std::sync::Arc;

use crate::keys::{match_key, TuiKey};
use crate::kill_ring::{KillRing, KillRingPushOptions};
use crate::tui::Component;
use crate::undo_stack::UndoStack;
use crate::utils::{slice_with_width, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward, segment_text, Segment, WordNavigationOptions};
use crate::autocomplete::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};
use crate::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};

/// Regex matching paste markers like `[paste #1 +123 lines]`.
fn paste_marker_regex() -> regex::Regex {
    regex::Regex::new(r"\[paste #(\d+)( (\+\d+ lines|\d+ chars))?\]").unwrap()
}

fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10 && paste_marker_regex().is_match(segment)
}

/// A chunk of text for word-wrap layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Whether a char starts a new grapheme (simplified extended grapheme
/// cluster rules covering combining marks, ZWJ and CRLF).
fn is_combining_mark(c: char) -> bool {
    // Unicode combining marks / variation selectors / ZWJ (non-exhaustive,
    // covers the common ranges used in terminal text).
    matches!(c as u32,
        0x0300..=0x036f | 0x0483..=0x0489 | 0x0591..=0x05bd | 0x05bf | 0x05c1..=0x05c2 | 0x05c4..=0x05c5
        | 0x0610..=0x061a | 0x064b..=0x065f | 0x0670 | 0x06d6..=0x06ed | 0x0711 | 0x0730..=0x074a
        | 0x07eb..=0x07f3 | 0x0816..=0x082d | 0x0859..=0x085b | 0x08d3..=0x0903 | 0x093a..=0x0957
        | 0x0962..=0x0963 | 0x0981..=0x0983 | 0x09bc..=0x09d7 | 0x0a01..=0x0a03 | 0x0a3c
        | 0x0a3e..=0x0a51 | 0x0a70..=0x0a75 | 0x0abc..=0x0acd | 0x0ae2..=0x0ae3 | 0x0b01..=0x0b57
        | 0x0b62..=0x0b63 | 0x0b82 | 0x0bbe..=0x0bcd | 0x0c00..=0x0c04 | 0x0c3e..=0x0c4d
        | 0x0c55..=0x0c56 | 0x0c62..=0x0c63 | 0x0c81..=0x0c83 | 0x0cbc | 0x0cbe..=0x0ccd
        | 0x0cd5..=0x0cd6 | 0x0ce2..=0x0ce3 | 0x0d00..=0x0d03 | 0x0d3b..=0x0d57 | 0x0d62..=0x0d63
        | 0x0d82..=0x0d83 | 0x0dca | 0x0dcf..=0x0ddf | 0x0df2..=0x0df3 | 0x0e31 | 0x0e34..=0x0e3a
        | 0x0e47..=0x0e4e | 0x0eb1 | 0x0eb4..=0x0ebc | 0x0ec8..=0x0ecd | 0x0f18..=0x0f19 | 0x0f35
        | 0x0f37 | 0x0f39 | 0x0f3e..=0x0f3f | 0x0f71..=0x0f97 | 0x0f99..=0x0fbc | 0x0fc6
        | 0x102b..=0x103e | 0x1056..=0x1060 | 0x1062..=0x1074 | 0x1082..=0x108d | 0x108f
        | 0x109a..=0x109d | 0x135d..=0x135f | 0x1712..=0x1714 | 0x1732..=0x1734 | 0x1752..=0x1753
        | 0x1772..=0x1773 | 0x17b4..=0x17d3 | 0x17dd | 0x180b..=0x180d | 0x1885..=0x1886 | 0x18a9
        | 0x1920..=0x193b | 0x1a17..=0x1a1b | 0x1a55..=0x1a7c | 0x1a7f | 0x1ab0..=0x1aff
        | 0x1b00..=0x1b04 | 0x1b34..=0x1b73 | 0x1b80..=0x1bad | 0x1be6..=0x1bf3 | 0x1c24..=0x1c37
        | 0x1cd0..=0x1ce8 | 0x1ced | 0x1cf2..=0x1cf4 | 0x1cf8..=0x1cf9 | 0x1dc0..=0x1dff
        | 0x200c | 0x20d0..=0x20f0 | 0x2cef..=0x2cf1 | 0x2d7f | 0x2de0..=0x2dff | 0x302a..=0x302f
        | 0x3099..=0x309a | 0xa66f..=0xa672 | 0xa674..=0xa67d | 0xa69e..=0xa69f | 0xa6f0..=0xa6f1
        | 0xa802 | 0xa806 | 0xa80b | 0xa823..=0xa827 | 0xa880..=0xa881 | 0xa8b4..=0xa8c5
        | 0xa8e0..=0xa8f1 | 0xa926..=0xa92d | 0xa947..=0xa953 | 0xa980..=0xa983 | 0xa9b3..=0xa9c0
        | 0xa9e5 | 0xaa29..=0xaa36 | 0xaa43 | 0xaa4c..=0xaa4d | 0xaa7b..=0xaa7d | 0xaab0
        | 0xaab2..=0xaab4 | 0xaab7..=0xaab8 | 0xaabe..=0xaabf | 0xaac1 | 0xaaeb..=0xaaef
        | 0xaaf5..=0xaaf6 | 0xabe3..=0xabea | 0xabec..=0xabed | 0xfb1e | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f | 0x101fd | 0x102e0 | 0x10376..=0x1037a | 0x10a01..=0x10a0f
        | 0x11000..=0x11002 | 0x11038..=0x11046 | 0x1107f..=0x11082 | 0x110b0..=0x110ba
        | 0x11100..=0x11102 | 0x11127..=0x11134 | 0x11173 | 0x11180..=0x11182 | 0x111b3..=0x111c0
        | 0x111ca..=0x111cc | 0x1122c..=0x11237 | 0x1123e | 0x112df..=0x112ea | 0x11300..=0x11303
        | 0x1133b..=0x11344 | 0x11347..=0x1134d | 0x11357 | 0x11362..=0x11363 | 0x11366..=0x1136c
        | 0x11370..=0x11374 | 0x11435..=0x11446 | 0x114b0..=0x114c3 | 0x115af..=0x115c0 | 0x115dc..=0x115dd
        | 0x11630..=0x11640 | 0x116ab..=0x116b7 | 0x1171d..=0x1172b | 0x1182c..=0x1183a
        | 0x119d1..=0x119e0 | 0x119e4 | 0x11a01..=0x11a0a | 0x11a33..=0x11a3e | 0x11a47
        | 0x11a51..=0x11a5b | 0x11a8a..=0x11a99 | 0x11c2f..=0x11c3f | 0x11c92..=0x11cb6
        | 0x11d31..=0x11d45 | 0x11d47 | 0x11d8a..=0x11d97 | 0x11ef3..=0x11ef6 | 0x16af0..=0x16af4
        | 0x16b30..=0x16b36 | 0x16f51..=0x16f7e | 0x16f8f..=0x16f92 | 0x1bc9d..=0x1bc9e
        | 0x1d165..=0x1d172 | 0x1d17b..=0x1d182 | 0x1d185..=0x1d18b | 0x1d1aa..=0x1d1ad
        | 0x1d242..=0x1d244 | 0x1da00..=0x1da36 | 0x1da3b..=0x1da6c | 0x1da75 | 0x1da84
        | 0x1da9b..=0x1da9f | 0x1daa1..=0x1daaf | 0x1e000..=0x1e02a | 0x1e8d0..=0x1e8d6
        | 0x1e944..=0x1e94a | 0xe0100..=0xe01ef)
}

fn is_grapheme_start(prev: Option<char>, next: Option<char>, c: char) -> bool {
    if let Some(_prev) = prev {
        if is_combining_mark(c) {
            return false;
        }
    }
    if prev == Some('\u{200d}') {
        return false; // ZWJ joins sequences
    }
    if c == '\u{200d}' {
        return false;
    }
    let _ = next;
    true
}

/// Segment text into grapheme-like units (each with index/byte width).
fn grapheme_segments(text: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let start = i;
        i += 1;
        while i < chars.len() && !is_grapheme_start(Some(chars[i - 1]), Some(chars.get(i + 1).copied().unwrap_or(' ')), chars[i]) {
            i += 1;
        }
        let seg: String = chars[start..i].iter().collect();
        let byte = text.char_indices().nth(start).map(|(b, _)| b).unwrap_or(text.len());
        segments.push(Segment { segment: seg, index: byte, is_word_like: true });
    }
    if segments.is_empty() {
        return segments;
    }
    // CRLF handling: attach '\n' to a preceding '\r' grapheme.
    segments
}

/// Segment with paste-marker awareness: markers whose ID exists in
/// `valid_ids` are merged into single atomic segments.
fn segment_with_markers(text: &str, base: &[Segment], valid_ids: &std::collections::HashSet<usize>) -> Vec<Segment> {
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
pub fn word_wrap_line(line: &str, max_width: usize, pre_segmented: Option<&[Segment]>) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk { text: String::new(), start_index: 0, end_index: 0 }];
    }
    let line_width = visible_width(line);
    if line_width <= max_width {
        return vec![TextChunk { text: line.to_string(), start_index: 0, end_index: line.len() }];
    }

    let segments: Vec<Segment> = match pre_segmented {
        Some(s) => s.to_vec(),
        None => grapheme_segments(line),
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
        let is_ws = !is_paste_marker(grapheme) && grapheme.chars().next().map(|c| c.is_whitespace()).unwrap_or(false);

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
                    || !nextseg.segment.chars().next().map(|c| c.is_whitespace()).unwrap_or(false)
                {
                    wrap_opp_index = nextseg.index as isize;
                    wrap_opp_width = current_width;
                }
            }
        } else if let Some(nextseg) = next {
            let next_ws = nextseg.segment.chars().next().map(|c| c.is_whitespace());
            if next_ws == Some(false) {
                let is_cjk = !is_paste_marker(grapheme) && is_cjk_segment(grapheme);
                let next_is_cjk = !is_paste_marker(&nextseg.segment) && is_cjk_segment(&nextseg.segment);
                if is_cjk || next_is_cjk {
                    wrap_opp_index = nextseg.index as isize;
                    wrap_opp_width = current_width;
                }
            }
        }
    }

    chunks.push(TextChunk { text: line[chunk_start..].to_string(), start_index: chunk_start, end_index: line.len() });
    chunks
}

fn is_cjk_segment(seg: &str) -> bool {
    seg.chars().next().map(crate::word_navigation::is_cjk_char).unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq)]
struct EditorState {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

impl EditorState {
    fn empty() -> Self {
        Self { lines: vec![String::new()], cursor_line: 0, cursor_col: 0 }
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
    EditorTheme { border_color: Arc::new(|s| s.to_string()) }
}

pub struct EditorOptions {
    pub padding_x: usize,
    pub autocomplete_max_visible: usize,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self { padding_x: 0, autocomplete_max_visible: 5 }
    }
}

fn create_scroll_border(direction: &str, hidden_line_count: usize, width: usize) -> String {
    let available_width = width;
    let indicator = format!("─── {direction} {hidden_line_count} more ");
    let indicator_w = visible_width(&indicator);
    if indicator_w <= available_width {
        return format!("{}{}", indicator, "─".repeat(available_width - indicator_w));
    }
    let ellipsis = "...";
    let indicator_width = available_width.saturating_sub(3);
    let sliced = slice_with_width(&indicator, indicator_width);
    format!("{sliced}{ellipsis}")
}

const SLASH_COMMAND_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
};

/// The editor component.
pub struct Editor {
    state: EditorState,
    pub focused: bool,
    padding_x: usize,
    last_width: usize,
    scroll_offset: std::cell::Cell<usize>,
    pub border_color: Arc<dyn Fn(&str) -> String + Send + Sync>,
    terminal_rows: usize,

    // Autocomplete
    autocomplete_provider: Option<Box<dyn AutocompleteProvider + Send + Sync>>,
    autocomplete_trigger_characters: Vec<String>,
    autocomplete_list: Option<SelectList>,
    autocomplete_state: Option<&'static str>,
    autocomplete_prefix: String,
    autocomplete_max_visible: usize,

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
        f.debug_struct("Editor").field("state", &self.state).finish()
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
            last_width: 80,
            scroll_offset: std::cell::Cell::new(0),
            border_color,
            terminal_rows,
            autocomplete_provider: None,
            autocomplete_trigger_characters: vec!["@".to_string(), "#".to_string()],
            autocomplete_list: None,
            autocomplete_state: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible,
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

    pub fn set_autocomplete_provider(&mut self, provider: Box<dyn AutocompleteProvider + Send + Sync>) {
        self.cancel_autocomplete();
        let triggers = provider.trigger_characters();
        self.autocomplete_provider = Some(provider);
        self.set_autocomplete_trigger_characters(triggers);
    }

    fn valid_paste_ids(&self) -> std::collections::HashSet<usize> {
        self.pastes.keys().copied().collect()
    }

    fn segment(&self, text: &str, mode: &str) -> Vec<Segment> {
        let base = if mode == "word" { segment_text(text) } else { grapheme_segments(text) };
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
        let visual = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let visual = self.build_visual_line_map(self.last_width);
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
                self.scroll_offset.set(0);
            } else {
                self.set_text_internal("", "end");
            }
        } else {
            let entry = self.history.get(self.history_index as usize).cloned().unwrap_or_default();
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
        self.state.cursor_line = if cursor_placement == "start" { 0 } else { self.state.lines.len() - 1 };
        let col = if cursor_placement == "start" {
            0
        } else {
            self.state.lines[self.state.cursor_line].len()
        };
        self.set_cursor_col(col);
        self.scroll_offset.set(0);
    }

    // ------------------------------------------------------------------ text access

    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

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
        text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
    }

    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = self.normalize_text(text);
        let inserted_lines: Vec<&str> = normalized.split('\n').collect();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        let before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        let after_cursor = current_line[self.state.cursor_col.min(current_line.len())..].to_string();

        if inserted_lines.len() == 1 {
            self.state.lines[self.state.cursor_line] = format!("{before_cursor}{normalized}{after_cursor}");
            self.set_cursor_col(self.state.cursor_col + normalized.len());
        } else {
            let mut new_lines: Vec<String> = Vec::new();
            new_lines.extend(self.state.lines[..self.state.cursor_line].iter().cloned());
            new_lines.push(format!("{before_cursor}{}", inserted_lines[0]));
            for mid in inserted_lines.iter().skip(1).take(inserted_lines.len().saturating_sub(2)) {
                new_lines.push(mid.to_string());
            }
            new_lines.push(format!("{}{after_cursor}", inserted_lines[inserted_lines.len() - 1]));
            new_lines.extend(self.state.lines[self.state.cursor_line + 1..].iter().cloned());
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
        let layout_width = std::cmp::max(1, content_width.saturating_sub(if padding_x > 0 { 0 } else { 1 }));

        let horizontal = (self.border_color)("─");
        let layout_lines = self.layout_text(layout_width);

        let terminal_rows = self.terminal_rows.max(5);
        let max_visible_lines = std::cmp::max(5, terminal_rows * 3 / 10);

        let cursor_line_index = layout_lines.iter().position(|l| l.has_cursor).unwrap_or(0);
        let scroll = self.scroll_offset.get();
        if cursor_line_index < scroll {
            self.scroll_offset.set(cursor_line_index);
        } else if cursor_line_index >= scroll + max_visible_lines {
            self.scroll_offset.set(cursor_line_index - max_visible_lines + 1);
        }
        let max_scroll_offset = layout_lines.len().saturating_sub(max_visible_lines);
        self.scroll_offset.set(self.scroll_offset.get().min(max_scroll_offset));

        let scroll = self.scroll_offset.get();
        let visible_lines = &layout_lines[scroll..scroll + max_visible_lines.min(layout_lines.len().saturating_sub(scroll))];

        let mut result: Vec<String> = Vec::new();
        let left_padding = " ".repeat(padding_x);
        let right_padding = left_padding.clone();

        let scroll = self.scroll_offset.get();
        if scroll > 0 {
            result.push((self.border_color)(&create_scroll_border("↑", scroll, width)));
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
                    let marker = if emit_cursor_marker { "\x1b_pi:c\x07" } else { "" };

                    if !after.is_empty() {
                        let gen = self.segment(&after, "grapheme");
                        let first_grapheme = gen.first().map(|s| s.segment.clone()).unwrap_or_default();
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
            let line_right_padding = if cursor_in_padding { right_padding[1..].to_string() } else { right_padding.clone() };
            result.push(format!("{left_padding}{display_text}{padding}{line_right_padding}"));
        }

        let scroll = self.scroll_offset.get();
        let lines_below = layout_lines.len().saturating_sub((scroll + visible_lines.len()).min(layout_lines.len()));
        if lines_below > 0 {
            result.push((self.border_color)(&create_scroll_border("↓", lines_below, width)));
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

        if self.state.lines.is_empty() || (self.state.lines.len() == 1 && self.state.lines[0].is_empty()) {
            layout_lines.push(LayoutLine { text: String::new(), has_cursor: true, cursor_pos: Some(0) });
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
                    layout_lines.push(LayoutLine { text: line.clone(), has_cursor: false, cursor_pos: None });
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
                            has_cursor_in_chunk = cursor_pos >= chunk.start_index && cursor_pos < chunk.end_index;
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
                        layout_lines.push(LayoutLine { text: chunk.text.clone(), has_cursor: false, cursor_pos: None });
                    }
                }
            }
        }

        layout_lines
    }
}


impl Editor {
    // ------------------------------------------------------------------ input

    pub fn handle_input(&mut self, data: &str) {
        // Keyboard map for common raw sequences is handled by the terminal
        // backend (key string); here data is a key string such as "a",
        // "enter", "ctrl+c", "up", "shift+enter".

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

        // Bracketed paste start.
        let mut data_mut = data.to_string();
        if data.contains("\x1b[200~") {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            data_mut = data.replace("\x1b[200~", "");
        }

        if self.is_in_paste {
            self.paste_buffer.push_str(&data_mut);
            if let Some(end_index) = self.paste_buffer.find("\x1b[201~") {
                let paste_content = self.paste_buffer[..end_index].to_string();
                if !paste_content.is_empty() {
                    self.handle_paste(&paste_content);
                }
                self.is_in_paste = false;
                let remaining = self.paste_buffer[end_index + 6..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
                return;
            }
            return;
        }

        // Ctrl+C -> copy (handled by the parent loop). Editor ignores.
        if matches_key(data, "ctrl+c") {
            return;
        }

        // Undo.
        if matches_key(data, "ctrl+-") {
            self.undo();
            return;
        }

        // Autocomplete mode.
        if self.autocomplete_state.is_some() && self.autocomplete_list.is_some() {
            if matches_key(data, "escape") || matches_key(data, "ctrl+c") {
                self.cancel_autocomplete();
                return;
            }
            if matches_key(data, "up") || matches_key(data, "down") {
                let key = TuiKey::parse_simple(data);
                if let Some(list) = &mut self.autocomplete_list {
                    list.handle_input(&key);
                }
                return;
            }
            if matches_key(data, "tab") {
                let selected = self.autocomplete_list.as_ref().and_then(|l| l.get_selected_item()).cloned();
                if let Some(selected) = selected {
                    self.push_undo_snapshot();
                    self.last_action = None;
                    self.apply_autocomplete_item(&selected);
                }
                return;
            }
            if matches_key(data, "enter") {
                let selected = self.autocomplete_list.as_ref().and_then(|l| l.get_selected_item()).cloned();
                if let Some(selected) = selected {
                    self.push_undo_snapshot();
                    self.last_action = None;
                    let slash = self.autocomplete_prefix.starts_with('/');
                    self.apply_autocomplete_item(&selected);
                    if !slash {
                        return;
                    }
                    // For slash-command completions, fall through to submit.
                }
            }
        }

        // Tab triggers completion.
        if matches_key(data, "tab") && self.autocomplete_state.is_none() {
            self.handle_tab_completion();
            return;
        }

        // Deletion actions.
        if matches_key(data, "ctrl+k") {
            self.delete_to_end_of_line();
            return;
        }
        if matches_key(data, "ctrl+u") {
            self.delete_to_start_of_line();
            return;
        }
        if matches_key(data, "ctrl+w") || matches_key(data, "alt+backspace") {
            self.delete_word_backwards();
            return;
        }
        if matches_key(data, "alt+d") || matches_key(data, "alt+delete") {
            self.delete_word_forward();
            return;
        }
        if matches_key(data, "backspace") || matches_key(data, "shift+backspace") {
            self.handle_backspace();
            return;
        }
        if matches_key(data, "delete") || matches_key(data, "ctrl+d") || matches_key(data, "shift+delete") {
            self.handle_forward_delete();
            return;
        }

        // Kill ring.
        if matches_key(data, "ctrl+y") {
            self.yank();
            return;
        }
        if matches_key(data, "alt+y") {
            self.yank_pop();
            return;
        }

        // Line start/end.
        if matches_key(data, "home") || matches_key(data, "ctrl+home") || matches_key(data, "ctrl+a") {
            self.move_to_line_start();
            return;
        }
        if matches_key(data, "end") || matches_key(data, "ctrl+end") || matches_key(data, "ctrl+e") {
            self.move_to_line_end();
            return;
        }
        if matches_key(data, "alt+left") || matches_key(data, "ctrl+left") || matches_key(data, "alt+b") {
            self.move_word_backwards();
            return;
        }
        if matches_key(data, "alt+right") || matches_key(data, "ctrl+right") || matches_key(data, "alt+f") {
            self.move_word_forwards();
            return;
        }

        // New line.
        if matches_key(data, "shift+enter") || matches_key(data, "ctrl+j") {
            self.add_new_line();
            return;
        }

        // Submit.
        if matches_key(data, "enter") {
            if self.disable_submit {
                return;
            }
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            if self.state.cursor_col > 0
                && current_line
                    .chars()
                    .nth(self.state.cursor_col.saturating_sub(1))
                    .map(|c| c == '\\')
                    .unwrap_or(false)
            {
                self.handle_backspace();
                self.add_new_line();
                return;
            }
            self.submit_value();
            return;
        }

        // Arrow keys with history support.
        if matches_key(data, "up") {
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
        if matches_key(data, "down") {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if matches_key(data, "right") {
            self.move_cursor(0, 1);
            return;
        }
        if matches_key(data, "left") {
            self.move_cursor(0, -1);
            return;
        }

        // Page up/down.
        if matches_key(data, "pageup") || matches_key(data, "ctrl+pageup") {
            self.page_scroll(-1);
            return;
        }
        if matches_key(data, "pagedown") || matches_key(data, "ctrl+pagedown") {
            self.page_scroll(1);
            return;
        }

        // Character jump mode triggers.
        if matches_key(data, "ctrl+]") {
            self.jump_mode = Some("forward");
            return;
        }
        if matches_key(data, "ctrl+alt+]") {
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

    // ------------------------------------------------------------------ editing

    fn insert_character(&mut self, char: &str) {
        self.exit_history_browsing();

        let is_ws = char.chars().next().map(|c| c.is_whitespace()).unwrap_or(false);
        if is_ws || self.last_action != Some("type-word") {
            self.push_undo_snapshot();
        }
        self.last_action = Some("type-word");

        let line = self.state.lines.get_mut(self.state.cursor_line).cloned().unwrap_or_default();
        let before = line[..self.state.cursor_col.min(line.len())].to_string();
        let after = line[self.state.cursor_col.min(line.len())..].to_string();
        self.state.lines[self.state.cursor_line] = format!("{before}{char}{after}");
        self.set_cursor_col(self.state.cursor_col + char.len());

        // Autocomplete triggers.
        if self.autocomplete_state.is_none() {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let text_before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            if char == "/" && self.is_at_start_of_message() {
                self.try_trigger_autocomplete(false);
            } else if self.autocomplete_trigger_characters.iter().any(|t| t == char) {
                let chars: Vec<char> = text_before_cursor.chars().collect();
                let char_before_symbol = if chars.len() >= 2 { Some(chars[chars.len() - 2]) } else { None };
                if text_before_cursor.chars().count() == 1
                    || char_before_symbol == Some(' ')
                    || char_before_symbol == Some('\t')
                {
                    self.try_trigger_autocomplete(false);
                }
            } else if char.chars().next().map(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).unwrap_or(false)
            {
                if self.is_in_slash_command_context(&text_before_cursor)
                    || self.autocomplete_trigger_pattern(&text_before_cursor)
                {
                    self.try_trigger_autocomplete(false);
                }
            }
        } else {
            self.update_autocomplete();
        }
    }

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
        self.autocomplete_trigger_characters.iter().any(|t| t == &first.to_string())
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
        if filtered_text.starts_with('/') || filtered_text.starts_with('~') || filtered_text.starts_with('.') {
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            let char_before = if self.state.cursor_col > 0 {
                current_line
                    .chars()
                    .nth(self.state.cursor_col.saturating_sub(1))
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
        let result = self.expand_paste_markers(&self.state.lines.join("\n")).trim().to_string();

        self.state = EditorState::empty();
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset.set(0);
        self.undo_stack.clear();
        self.last_action = None;
        self.submit_pending = Some(result);
    }

    /// Drain a pending submitted prompt (None when none pending).
    pub fn drain_submitted(&mut self) -> Option<String> {
        self.submit_pending.take()
    }

    fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();

            let mut line = self.state.lines[self.state.cursor_line].clone();
            let before_cursor = line[..self.state.cursor_col.min(line.len())].to_string();

            let graphemes = self.segment(&before_cursor, "grapheme");
            let last_grapheme = graphemes.last().cloned();
            let grapheme_len = last_grapheme.as_ref().map(|g| g.segment.len()).unwrap_or(1);

            // Paste-marker handling: backspace removes the marker + registry.
            if let Some(g) = &last_grapheme {
                if is_paste_marker(&g.segment) {
                    if let Some(cap) = paste_marker_regex().captures(&g.segment) {
                        let target_id: usize = cap.get(1).unwrap().as_str().parse().unwrap_or(0);
                        self.pastes.remove(&target_id);
                        self.paste_counter = self.paste_counter.saturating_sub(1);
                        // Renumber higher ids down by one.
                        let mut higher: Vec<usize> = self.pastes.keys().copied().filter(|id| *id > target_id).collect();
                        higher.sort();
                        for id in higher {
                            if let Some(content) = self.pastes.remove(&id) {
                                self.pastes.insert(id - 1, content);
                            }
                        }
                        // Renumber markers in text.
                        let mut renumbered: Vec<String> = Vec::new();
                        for line in &self.state.lines {
                            let re = paste_marker_regex();
                            let updated = re
                                .replace_all(line, |caps: &regex::Captures| {
                                    let x: usize = caps.get(1).unwrap().as_str().parse().unwrap_or(0);
                                    if x <= target_id {
                                        return caps.get(0).unwrap().as_str().to_string();
                                    }
                                    let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                                    format!("[paste #{}{suffix}]", x - 1)
                                })
                                .to_string();
                            renumbered.push(updated);
                        }
                        self.state.lines = renumbered;
                    }
                }
            }

            line = self.state.lines[self.state.cursor_line].clone();
            let cursor_col = self.state.cursor_col;
            let before = line[..cursor_col.saturating_sub(grapheme_len).min(line.len())].to_string();
            let after = line[cursor_col.min(line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col.saturating_sub(grapheme_len));
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
            let text_before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            if self.is_in_slash_command_context(&text_before_cursor) || self.autocomplete_trigger_pattern(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }
}

impl Editor {
    // ------------------------------------------------------------------ deletion & lines

    fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let deleted_text = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            self.kill_ring.push(&deleted_text, KillRingPushOptions { prepend: true, accumulate: self.last_action == Some("kill") });
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            self.kill_ring.push("\n", KillRingPushOptions { prepend: true, accumulate: self.last_action == Some("kill") });
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
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let deleted_text = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.kill_ring.push(&deleted_text, KillRingPushOptions { prepend: false, accumulate: self.last_action == Some("kill") });
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();
            self.kill_ring.push("\n", KillRingPushOptions { prepend: false, accumulate: self.last_action == Some("kill") });
            self.last_action = Some("kill");
            let next_line = self.state.lines[self.state.cursor_line + 1].clone();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }
    }

    fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo_snapshot();
                self.kill_ring.push("\n", KillRingPushOptions { prepend: true, accumulate: self.last_action == Some("kill") });
                self.last_action = Some("kill");
                let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
                self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
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
            let deleted_text = current_line[delete_from.min(current_line.len())..self.state.cursor_col.min(current_line.len())].to_string();
            self.kill_ring.push(&deleted_text, KillRingPushOptions { prepend: true, accumulate: was_kill });
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &current_line[..delete_from.min(current_line.len())], &current_line[self.state.cursor_col.min(current_line.len())..]);
            self.set_cursor_col(delete_from);
        }
    }

    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col >= current_line.len() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.push_undo_snapshot();
                self.kill_ring.push("\n", KillRingPushOptions { prepend: false, accumulate: self.last_action == Some("kill") });
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
            let deleted_text = current_line[self.state.cursor_col.min(current_line.len())..delete_to.min(current_line.len())].to_string();
            self.kill_ring.push(&deleted_text, KillRingPushOptions { prepend: false, accumulate: was_kill });
            self.last_action = Some("kill");
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &current_line[..self.state.cursor_col.min(current_line.len())], &current_line[delete_to.min(current_line.len())..]);
        }
    }

    fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let after_cursor = &current_line[self.state.cursor_col.min(current_line.len())..];
            let graphemes = self.segment(after_cursor, "grapheme");
            let first_grapheme = graphemes.first().cloned();
            let grapheme_len = first_grapheme.map(|g| g.segment.len()).unwrap_or(1);
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line[(self.state.cursor_col + grapheme_len).min(current_line.len())..].to_string();
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
            let text_before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            if self.is_in_slash_command_context(&text_before_cursor) || self.autocomplete_trigger_pattern(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    fn move_to_line_end(&mut self) {
        self.last_action = None;
        let len = self.state.lines.get(self.state.cursor_line).map(|l| l.len()).unwrap_or(0);
        self.set_cursor_col(len);
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

    fn find_visual_line_at(&self, visual_lines: &[(usize, usize, usize)], line: usize, col: usize) -> usize {
        let last = visual_lines.len().saturating_sub(1);
        for (i, vl) in visual_lines.iter().enumerate() {
            if vl.0 != line {
                continue;
            }
            let offset = col as isize - vl.1 as isize;
            let is_last_segment = i == visual_lines.len() - 1 || visual_lines[i + 1].0 != vl.0;
            if offset >= 0 && (offset < vl.2 as isize || (is_last_segment && offset == vl.2 as isize)) {
                return i;
            }
        }
        last
    }

    fn find_current_visual_line(&self, visual_lines: &[(usize, usize, usize)]) -> usize {
        self.find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    fn move_to_visual_line(&mut self, visual_lines: &[(usize, usize, usize)], current_visual_line: usize, target_visual_line: usize) {
        let Some(&(tgt_line, tgt_start, tgt_len)) = visual_lines.get(target_visual_line) else { return };
        let (cur_line, cur_start, _cur_len) = visual_lines[current_visual_line];

        let current_visual_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let vl_index = self.find_visual_line_at(visual_lines, cur_line, snapped);
                snapped.saturating_sub(visual_lines[vl_index].1)
            }
            None => self.state.cursor_col.saturating_sub(cur_start),
        };

        let is_last_source = current_visual_line == visual_lines.len() - 1
            || visual_lines[current_visual_line + 1].0 != cur_line;
        let source_max = if is_last_source { _cur_len } else { _cur_len.saturating_sub(1) };
        let is_last_target = target_visual_line == visual_lines.len() - 1
            || visual_lines.get(target_visual_line + 1).map(|v| v.0).unwrap_or(tgt_line + 1) != tgt_line;
        let target_max = if is_last_target { tgt_len } else { tgt_len.saturating_sub(1) };

        let move_to_col = self.compute_vertical_move_column(current_visual_col, source_max, target_max);

        self.state.cursor_line = tgt_line;
        let target_col = tgt_start + move_to_col;
        let logical_line = self.state.lines.get(tgt_line).cloned().unwrap_or_default();
        self.state.cursor_col = target_col.min(logical_line.len());

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
                self.snapped_from_cursor_col = Some(self.state.cursor_col);
                self.state.cursor_col = seg.index;
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    fn compute_vertical_move_column(&mut self, current_visual_col: usize, source_max: usize, target_max: usize) -> usize {
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
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current_visual_line = self.find_current_visual_line(&visual_lines);

        if delta_line != 0 {
            let target = current_visual_line as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_visual_line, target as usize);
            }
        }

        if delta_col != 0 {
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            if delta_col > 0 {
                if self.state.cursor_col < current_line.len() {
                    let after_cursor = &current_line[self.state.cursor_col.min(current_line.len())..];
                    let graphemes = self.segment(after_cursor, "grapheme");
                    let first = graphemes.first().cloned();
                    self.set_cursor_col(self.state.cursor_col + first.map(|g| g.segment.len()).unwrap_or(1));
                } else if self.state.cursor_line < self.state.lines.len() - 1 {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else {
                    if let Some(vl) = visual_lines.get(current_visual_line) {
                        self.preferred_visual_col = Some(self.state.cursor_col.saturating_sub(vl.1));
                    }
                }
            } else {
                if self.state.cursor_col > 0 {
                    let before_cursor = &current_line[..self.state.cursor_col.min(current_line.len())];
                    let graphemes = self.segment(before_cursor, "grapheme");
                    let last = graphemes.last().cloned();
                    self.set_cursor_col(self.state.cursor_col.saturating_sub(last.map(|g| g.segment.len()).unwrap_or(1)));
                } else if self.state.cursor_line > 0 {
                    self.state.cursor_line -= 1;
                    let prev_len = self.state.lines.get(self.state.cursor_line).map(|l| l.len()).unwrap_or(0);
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
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);
        let target = (current as isize + direction * page_size as isize)
            .clamp(0, visual_lines.len().saturating_sub(1) as isize) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    // ------------------------------------------------------------------ word navigation

    fn move_word_backwards(&mut self) {
        self.last_action = None;
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let prev_len = self.state.lines.get(self.state.cursor_line).map(|l| l.len()).unwrap_or(0);
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
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();

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
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{text}{after}");
            self.set_cursor_col(self.state.cursor_col + text.len());
        } else {
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            let before = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
            let after = current_line[self.state.cursor_col.min(current_line.len())..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{}", lines[0]);
            for (idx, line) in lines.iter().skip(1).take(lines.len().saturating_sub(2)).enumerate() {
                self.state.lines.insert(self.state.cursor_line + idx + 1, line.to_string());
            }
            let last_line_index = self.state.cursor_line + lines.len() - 1;
            self.state.lines.insert(last_line_index, format!("{}{after}", lines[lines.len() - 1]));
            self.state.cursor_line = last_line_index;
            self.set_cursor_col(lines[lines.len() - 1].len());
        }
    }

    fn delete_yanked_text(&mut self) {
        let yanked_text = self.kill_ring.peek().map(|s| s.to_string());
        let Some(yanked_text) = yanked_text else { return };
        let yank_lines: Vec<&str> = yanked_text.split('\n').collect();

        if yank_lines.len() == 1 {
            let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
            let delete_len = yanked_text.len();
            let before = current_line[..self.state.cursor_col.saturating_sub(delete_len).min(current_line.len())].to_string();
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
            self.state.lines.insert(start_line, format!("{before_yank}{after_cursor}"));
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
        let Some(snapshot) = self.undo_stack.pop() else { return };
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
                let search_from = if is_current { self.state.cursor_col + 1 } else { 0 };
                line[search_from.min(line.len())..].find(ch).map(|i| search_from.min(line.len()) + i)
            } else {
                let search_from = if is_current {
                    self.state.cursor_col.saturating_sub(1).min(line.len())
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
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
        let before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
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
        let current_line = self.state.lines.get(self.state.cursor_line).cloned().unwrap_or_default();
        let before_cursor = current_line[..self.state.cursor_col.min(current_line.len())].to_string();
        if self.is_in_slash_command_context(&before_cursor) && !before_cursor.trim_start().contains(' ') {
            self.request_autocomplete(false, true);
        } else {
            self.force_file_autocomplete(true);
        }
    }

    fn force_file_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(true, explicit_tab);
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = self.autocomplete_provider.take() else { return };

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

        let aborted = std::sync::atomic::AtomicBool::new(false);
        let suggestions = provider.get_suggestions(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            force,
            &aborted,
        );

        match suggestions {
            None => {
                self.autocomplete_provider = Some(provider);
                self.cancel_autocomplete();
            }
            Some(suggestions) => {
                if suggestions.items.is_empty() {
                    self.autocomplete_provider = Some(provider);
                    self.cancel_autocomplete();
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
                self.apply_autocomplete_suggestions(suggestions, if force { "force" } else { "regular" });
            }
        }
    }

    fn apply_autocomplete_suggestions(&mut self, suggestions: AutocompleteSuggestions, state: &'static str) {
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
        self.state.lines = result.lines;
        self.state.cursor_line = result.cursor_line;
        self.set_cursor_col(result.cursor_col);
        self.cancel_autocomplete();
    }

    fn apply_autocomplete_item(&mut self, selected: &SelectItem) {
        let Some(provider) = self.autocomplete_provider.take() else { return };
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
        self.request_autocomplete(force, false);
    }

    fn cancel_autocomplete(&mut self) {
        self.autocomplete_state = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    pub fn current_autocomplete_selection(&self) -> Option<SelectItem> {
        self.autocomplete_list.as_ref().and_then(|l| l.get_selected_item().cloned())
    }
}

// ------------------------------------------------------------------ helper fns

/// True when `data` matches a key-string pattern (terminal-normalized surface).
fn matches_key(data: &str, pattern: &str) -> bool {
    let key = TuiKey::parse_simple(data);
    match_key(&key, pattern)
}

fn matches_jump_cancel(data: &str) -> bool {
    matches_key(data, "ctrl+]") || matches_key(data, "ctrl+alt+]")
}

fn decode_printable(data: &str) -> Option<String> {
    let c = data.chars().next()?;
    if data.len() == c.len_utf8() && (c as u32) >= 32 && !c.is_control() {
        if matches_key(data, "enter") || matches_key(data, "tab") || matches_key(data, "backspace")
            || matches_key(data, "delete") || matches_key(data, "up") || matches_key(data, "down")
            || matches_key(data, "left") || matches_key(data, "right")
        {
            return None;
        }
        return Some(c.to_string());
    }
    // Single-letter keys typed as "a" are handled above; multi-char names are
    // non-printable control key strings.
    None
}

/// Decode CSI-u Ctrl+<letter> sequences inside bracketed paste back to bytes.
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
}

#[path = "editor_tests.rs"]
#[cfg(test)]
mod editor_tests;
