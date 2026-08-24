//! Alternate-screen overlay components — port of
//! `packages/tui/src/components/alt-screen-flash.ts` and
//! `packages/tui/src/alt-screen-search.ts`.
//!
//! - `AltScreenFlashContainer`: transient reversed-video messages.
//! - `AltScreenSearchComponent`: a find-in-transcript input bar plus pure
//!   match computation (`find_alt_screen_search_matches`).

use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::{strip_ansi_codes, truncate_to_width, visible_width};

/// A transient flash message. The Rust port exposes `flash` with an explicit
/// duration; the interactive loop removes expired entries when it ticks.
#[derive(Debug, Clone)]
pub struct FlashEntry {
    pub id: u64,
    pub message: String,
    pub expires_at: std::time::Instant,
}

/// Container of transient flash messages.
#[derive(Debug, Default)]
pub struct AltScreenFlashContainer {
    entries: Vec<FlashEntry>,
    next_id: u64,
}

impl AltScreenFlashContainer {
    pub fn new() -> Self {
        Self::default()
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
        id
    }

    /// Remove expired entries; returns whether any were removed.
    pub fn prune_expired(&mut self, now: std::time::Instant) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.expires_at > now);
        self.entries.len() != before
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

fn append_mapped_text(
    text: &str,
    span: Option<SearchSegment>,
    corpus: &mut (String, Vec<Option<SearchSegment>>),
) {
    corpus.0.push_str(text);
    for _ in 0..text.chars().count() {
        corpus.1.push(span.clone());
    }
}

fn build_search_corpus(lines: &[String]) -> (String, Vec<Option<SearchSegment>>) {
    let mut corpus: (String, Vec<Option<SearchSegment>>) = (String::new(), Vec::new());
    let mut pending_separator = false;

    for (row, line) in lines.iter().enumerate() {
        let clean = strip_ansi_codes(line);
        let mut column = 0usize;
        let chars: Vec<char> = clean.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let mut text: String = chars[i..]
                .iter()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if text.is_empty() {
                text = chars[i].to_string();
            }
            let width = text.chars().count();
            if text.chars().all(|c| c.is_whitespace()) {
                if !corpus.0.is_empty() {
                    pending_separator = true;
                }
                column += width;
                i += text.chars().count();
                continue;
            }
            if pending_separator {
                append_mapped_text(" ", None, &mut corpus);
                pending_separator = false;
            }
            append_mapped_text(
                &text,
                Some(SearchSegment {
                    row,
                    start_col: column,
                    end_col: column + width,
                }),
                &mut corpus,
            );
            column += width;
            i += text.chars().count();
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
    let normalized_query = normalize_query(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }

    let (text, source) = build_search_corpus(lines);
    let mut matches: Vec<SearchMatch> = Vec::new();

    let mut start = 0usize;
    while let Some(rel) = text[start..].find(&normalized_query) {
        let mstart = start + rel;
        let mend = mstart + normalized_query.chars().count();
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
}

impl Default for AltScreenSearchComponent {
    fn default() -> Self {
        Self {
            input: crate::components::input::Input::new(""),
            result_count: 0,
            result_index: -1,
        }
    }
}

impl AltScreenSearchComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the current match result position.
    pub fn set_result(&mut self, index: isize, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    pub fn query(&self) -> String {
        self.input.value.clone()
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
        vec![
            format!("\x1b[7m{title}{padding}\x1b[27m"),
            self.input.render(safe_width)[0].to_string(),
        ]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        self.input.handle_input(key);
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }
}

#[path = "extra_tests.rs"]
#[cfg(test)]
mod extra_tests;
