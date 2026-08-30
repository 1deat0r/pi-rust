//! Alternate-screen overlay components — port of
//! `packages/tui/src/components/alt-screen-flash.ts` and
//! `packages/tui/src/alt-screen-search.ts`.
//!
//! - `AltScreenFlashContainer`: transient reversed-video messages.
//! - `AltScreenSearchComponent`: a find-in-transcript input bar plus pure
//!   match computation (`find_alt_screen_search_matches`).

use crate::components::loader::RequestRenderFn;
use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::{
    grapheme_boundaries, strip_terminal_sequences, truncate_to_width, visible_width,
};

/// A transient flash message. The Rust port exposes `flash` with an explicit
/// duration; the interactive loop removes expired entries when it ticks.
#[derive(Debug, Clone)]
pub struct FlashEntry {
    pub id: u64,
    pub message: String,
    pub expires_at: std::time::Instant,
}

/// Container of transient flash messages.
#[derive(Default)]
pub struct AltScreenFlashContainer {
    entries: Vec<FlashEntry>,
    next_id: u64,
    request_render: Option<RequestRenderFn>,
}

impl std::fmt::Debug for AltScreenFlashContainer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AltScreenFlashContainer")
            .field("entries", &self.entries)
            .field("next_id", &self.next_id)
            .field("has_request_render", &self.request_render.is_some())
            .finish()
    }
}

impl AltScreenFlashContainer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a flash container with the thread-safe repaint signal used
    /// by the upstream `AltScreenFlashContainer`.
    pub fn with_request_render<F>(request_render: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self {
            request_render: Some(std::sync::Arc::new(request_render)),
            ..Self::default()
        }
    }

    /// Install or replace the repaint signal. The callback should only wake
    /// the owning event loop; it must not mutate terminal state directly.
    pub fn set_request_render_callback(&mut self, callback: Option<RequestRenderFn>) {
        self.request_render = callback;
    }

    /// Convenience setter for a thread-safe repaint signal.
    pub fn set_request_render<F>(&mut self, request_render: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.set_request_render_callback(Some(std::sync::Arc::new(request_render)));
    }

    fn request_render(&self) {
        if let Some(request_render) = &self.request_render {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                request_render();
            }));
        }
    }

    /// Add a flash message (returns the entry id).
    pub fn flash(&mut self, message: impl Into<String>, duration_ms: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(FlashEntry {
            id,
            message: message.into(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_millis(duration_ms),
        });
        self.request_render();
        id
    }

    /// Remove expired entries; returns whether any were removed.
    pub fn prune_expired(&mut self, now: std::time::Instant) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.expires_at > now);
        let removed = self.entries.len() != before;
        if removed {
            self.request_render();
        }
        removed
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Component for AltScreenFlashContainer {
    fn render(&self, width: usize) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                let message = truncate_to_width(&format!(" {} ", entry.message), width, "");
                format!("\x1b[7m{message}\x1b[27m")
            })
            .collect()
    }

    fn handle_input(&mut self, _key: &TuiKey) {
        // Ignore input; the container is passive.
    }
}

// ---------------------------------------------------------------------------
// Alt-screen search
// ---------------------------------------------------------------------------

/// A search span in the corpus mapping to a source row/column range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// A match: one or more segments across the source lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub segments: Vec<SearchSegment>,
}

/// A terminal-cell point used by transcript selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: usize,
    pub column: usize,
}

/// An ordered or reverse-ordered range of terminal-cell points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
}

fn ordered_points(a: SelectionPoint, b: SelectionPoint) -> (SelectionPoint, SelectionPoint) {
    if (a.row, a.column) <= (b.row, b.column) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Snap a pointer column to a complete terminal grapheme. Wide and combining
/// characters can occupy multiple cells, so a start point snaps left and an
/// end point snaps right when the pointer lands inside one grapheme.
pub fn snap_selection_column(line: &str, column: usize, end_point: bool) -> usize {
    let clean = strip_terminal_sequences(line);
    let mut visual_column = 0;
    for (start, end) in grapheme_boundaries(&clean) {
        let width = visible_width(&clean[start..end]);
        let next = visual_column + width;
        if column < next {
            return if end_point { next } else { visual_column };
        }
        visual_column = next;
    }
    visual_column
}

/// Extract a selection from rendered lines without splitting ANSI graphemes.
/// Terminal control sequences are omitted from copied text, matching the
/// transcript clipboard contract.
pub fn extract_selection(lines: &[String], range: SelectionRange) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (start, end) = ordered_points(range.start, range.end);
    let mut selected = Vec::new();
    for (row, line) in lines
        .iter()
        .enumerate()
        .skip(start.row)
        .take(end.row.saturating_sub(start.row).saturating_add(1))
    {
        let clean = strip_terminal_sequences(line);
        let line_width = visible_width(&clean);
        let from = if row == start.row {
            snap_selection_column(&clean, start.column, false)
        } else {
            0
        };
        let to = if row == end.row {
            snap_selection_column(&clean, end.column, true)
        } else {
            line_width
        };
        selected.push(crate::utils::slice_by_column(
            &clean,
            from,
            to.saturating_sub(from),
        ));
    }
    selected.join("\n")
}

type QueryChangeCallback = Box<dyn Fn(&str) + Send + Sync>;

fn append_mapped_text(
    text: &str,
    span: Option<SearchSegment>,
    corpus: &mut (String, Vec<Option<SearchSegment>>),
) {
    // Upstream searches with a Unicode, case-insensitive regular expression
    // (`giu`).  Fold both the corpus and query to keep byte offsets usable
    // while preserving the source span for every byte produced by folding.
    let folded = text.to_lowercase();
    corpus.0.push_str(&folded);
    // Rust's string search returns byte offsets.  Keep one source entry per
    // UTF-8 byte, matching JavaScript's UTF-16 source table closely enough
    // for every valid match while allowing CJK and emoji to map correctly.
    for _ in 0..folded.len() {
        corpus.1.push(span.clone());
    }
}

fn build_search_corpus(lines: &[String]) -> (String, Vec<Option<SearchSegment>>) {
    let mut corpus: (String, Vec<Option<SearchSegment>>) = (String::new(), Vec::new());
    let mut pending_separator = false;

    for (row, line) in lines.iter().enumerate() {
        let clean = strip_terminal_sequences(line);
        let mut column = 0usize;
        for (start, end) in grapheme_boundaries(&clean) {
            let text = &clean[start..end];
            let width = visible_width(text);
            if text.chars().all(char::is_whitespace) {
                if !corpus.0.is_empty() {
                    pending_separator = true;
                }
                column += width;
                continue;
            }
            if pending_separator {
                append_mapped_text(" ", None, &mut corpus);
                pending_separator = false;
            }
            append_mapped_text(
                text,
                Some(SearchSegment {
                    row,
                    start_col: column,
                    end_col: column + width,
                }),
                &mut corpus,
            );
            column += width;
        }
        if !corpus.0.is_empty() {
            pending_separator = true;
        }
    }

    corpus
}

fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Find search matches in rendered lines (upstream `findAltScreenSearchMatches`).
pub fn find_alt_screen_search_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    let normalized_query = normalize_query(query).to_lowercase();
    if normalized_query.is_empty() {
        return Vec::new();
    }

    let (text, source) = build_search_corpus(lines);
    let mut matches: Vec<SearchMatch> = Vec::new();

    let mut start = 0usize;
    while let Some(rel) = text[start..].find(&normalized_query) {
        let mstart = start + rel;
        let mend = mstart + normalized_query.len();
        let mut segments: Vec<SearchSegment> = Vec::new();
        for index in mstart..mend {
            let span = source.get(index).cloned().flatten();
            let Some(span) = span else { continue };
            if let Some(prev) = segments.last_mut() {
                if prev.row == span.row && span.start_col <= prev.end_col {
                    prev.end_col = prev.end_col.max(span.end_col);
                    continue;
                }
            }
            segments.push(span);
        }
        if !segments.is_empty() {
            matches.push(SearchMatch { segments });
        }
        start = mend;
    }

    matches
}

/// A stable dedup key for a match.
pub fn search_match_key(m: &SearchMatch) -> String {
    let first = m.segments.first();
    let last = m.segments.last();
    match (first, last) {
        (Some(f), Some(l)) => format!("{}:{}:{}:{}", f.row, f.start_col, l.row, l.end_col),
        _ => String::new(),
    }
}

/// Search input bar rendered as a reversed-video title row plus an input.
pub struct AltScreenSearchComponent {
    input: crate::components::input::Input,
    result_count: usize,
    result_index: isize,
    focused: bool,
    on_query_change: Option<QueryChangeCallback>,
}

impl Default for AltScreenSearchComponent {
    fn default() -> Self {
        Self {
            // Upstream constructs `new Input()`, whose renderer uses the
            // canonical `> ` prompt for the query row.
            input: crate::components::input::Input::new("> "),
            result_count: 0,
            result_index: -1,
            focused: false,
            on_query_change: None,
        }
    }
}

impl AltScreenSearchComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the upstream query-change callback while retaining the
    /// no-argument constructor used by the existing Rust callers.
    pub fn with_query_callback(mut self, callback: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_query_change = Some(Box::new(callback));
        self
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Update the current match result position.
    pub fn set_result(&mut self, index: isize, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    pub fn query(&self) -> String {
        self.input.value.clone()
    }

    pub fn set_query(&mut self, query: &str) {
        self.input.set_value(query);
        if let Some(callback) = &self.on_query_change {
            callback(query);
        }
    }
}

impl Component for AltScreenSearchComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let safe_width = std::cmp::max(1, width);
        let label = " Find transcript";
        let query = &self.input.value;
        let status = if query.is_empty() {
            String::new()
        } else if self.result_count == 0 {
            "No matches ".to_string()
        } else {
            format!("{}/{} ", self.result_index.max(0) + 1, self.result_count)
        };
        let label_width = visible_width(label);
        let status_width = visible_width(&status);
        let gap = " ".repeat(std::cmp::max(
            1,
            safe_width.saturating_sub(label_width + status_width),
        ));
        let title = truncate_to_width(&format!("{label}{gap}{status}"), safe_width, "");
        let padding = " ".repeat(safe_width.saturating_sub(visible_width(&title)));
        let mut lines = vec![format!("\x1b[7m{title}{padding}\x1b[27m")];
        lines.extend(self.input.render(safe_width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let previous = self.input.value.clone();
        self.input.handle_input(key);
        if self.input.value != previous {
            if let Some(callback) = &self.on_query_change {
                callback(&self.input.value);
            }
        }
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

#[path = "extra_tests.rs"]
#[cfg(test)]
mod extra_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn flash_repaints_on_insert_and_only_after_actual_expiry() {
        let repaint_count = Arc::new(AtomicUsize::new(0));
        let repaint_count_for_callback = Arc::clone(&repaint_count);
        let mut flashes = AltScreenFlashContainer::with_request_render(move || {
            repaint_count_for_callback.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(flashes.flash("ready", 0), 0);
        assert_eq!(repaint_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            flashes.render(20),
            vec!["\x1b[7m ready \x1b[27m".to_string()]
        );

        assert!(flashes.prune_expired(std::time::Instant::now()));
        assert_eq!(repaint_count.load(Ordering::SeqCst), 2);
        assert!(!flashes.prune_expired(std::time::Instant::now()));
        assert_eq!(repaint_count.load(Ordering::SeqCst), 2);
        assert!(flashes.render(20).is_empty());
    }

    #[test]
    fn flash_repaint_callback_can_be_disabled_without_changing_lifecycle() {
        let repaint_count = Arc::new(AtomicUsize::new(0));
        let repaint_count_for_callback = Arc::clone(&repaint_count);
        let mut flashes = AltScreenFlashContainer::with_request_render(move || {
            repaint_count_for_callback.fetch_add(1, Ordering::SeqCst);
        });
        flashes.set_request_render_callback(None);

        assert_eq!(flashes.flash("quiet", 0), 0);
        assert_eq!(repaint_count.load(Ordering::SeqCst), 0);
        assert!(flashes.prune_expired(std::time::Instant::now()));
        assert_eq!(repaint_count.load(Ordering::SeqCst), 0);

        flashes.clear();
        assert!(flashes.render(20).is_empty());
    }
}
