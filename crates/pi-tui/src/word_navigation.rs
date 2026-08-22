//! Word navigation — port of `packages/tui/src/word-navigation.ts`.
//!
//! A Rust approximation of the upstream `Intl.Segmenter` word segmentation:
//! whitespace runs, punctuation runs, CJK runs, and other word-like runs
//! (letters/digits/script chars) each form one segment. `option::segment`
//! can override segmentation (the editor uses it for paste-marker atomicity).

use std::collections::VecDeque;

pub const PUNCTUATION_REGEX: &str = "(){}[]<>.,;:'\"!?+-*/\\|&%^$#@~`";

pub fn is_punctuation_char(c: char) -> bool {
    PUNCTUATION_REGEX.chars().any(|p| p == c)
}

/// CJK scripts (Han/Hiragana/Katakana/Hangul/Bopomofo) — upstream `cjkBreakRegex`.
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{2EFF}' | '\u{3000}'..='\u{303F}' | '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'
        | '\u{3100}'..='\u{312F}' | '\u{31A0}'..='\u{31BF}' | '\u{31F0}'..='\u{31FF}' | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}' | '\u{A960}'..='\u{A97F}' | '\u{AC00}'..='\u{D7AF}' | '\u{F900}'..='\u{FAFF}'
        | '\u{FF66}'..='\u{FF9D}')
}

/// A single segment: its text and whether it is word-like.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub segment: String,
    /// Byte offset of the segment in the input (analogous to `Intl.SegmentData.index`).
    pub index: usize,
    pub is_word_like: bool,
}

/// Default (no-op) segmentation hook used when `WordNavigationOptions.segment`
/// is absent.
pub fn default_segment(text: &str) -> Vec<Segment> {
    segment_text(text)
}

/// Segment text into word/non-word runs matching the upstream `Segmenter`
/// behavior used by `findWordBackward`/`findWordForward`.
pub fn segment_text(text: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let start = i;
        let kind = if chars[i].is_whitespace() {
            0
        } else if is_punctuation_char(chars[i]) {
            1
        } else if is_cjk_char(chars[i]) {
            2
        } else {
            3
        };
        // For word-like runs, absorb combining marks and ZWJ+emoji-ish
        // continuation so graphemes stay together.
        while i < chars.len() {
            let c = chars[i];
            match kind {
                0 => {
                    if !c.is_whitespace() {
                        break;
                    }
                }
                1 => {
                    if !is_punctuation_char(c) {
                        break;
                    }
                }
                2 => {
                    if !is_cjk_char(c) {
                        break;
                    }
                }
                _ => {
                    if c.is_whitespace() || is_punctuation_char(c) || is_cjk_char(c) {
                        break;
                    }
                }
            }
            i += 1;
        }
        let seg_text: String = chars[start..i].iter().collect();
        let index_before = text.char_indices().nth(start).map(|(b, _)| b).unwrap_or(text.len());
        segments.push(Segment {
            segment: seg_text,
            index: index_before,
            is_word_like: kind == 2 || kind == 3,
        });
    }
    segments
}

/// A word is "word-like" and contains no punctuation (fast path used by the
/// editor and word navigation for whole-segment skips).
fn segment_has_no_punctuation(segment: &str) -> bool {
    !segment.chars().any(is_punctuation_char)
}

/// A custom segmenter returning word segments for the given text.
pub type SegmentFn<'a> = &'a dyn Fn(&str) -> Vec<Segment>;
/// A predicate identifying atomic segments treated as single units.
pub type AtomicSegmentFn<'a> = &'a dyn Fn(&str) -> bool;

/// Options for word navigation functions.
#[derive(Default)]
pub struct WordNavigationOptions<'a> {
    /// Custom segmenter returning word segments for the given text.
    pub segment: Option<SegmentFn<'a>>,
    /// Predicate identifying atomic segments that should be treated as
    /// single units (e.g. paste markers).
    pub is_atomic_segment: Option<AtomicSegmentFn<'a>>,
}

fn last_index_of_punctuation(segment: &str) -> Option<usize> {
    let mut last: Option<usize> = None;
    for (i, c) in segment.char_indices() {
        if is_punctuation_char(c) {
            last = Some(i);
        }
    }
    last
}


fn prev_char_boundary(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut i = cursor;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Find the cursor position after moving one word backward.
pub fn find_word_backward(text: &str, cursor: usize, options: &WordNavigationOptions) -> usize {
    if cursor == 0 {
        return 0;
    }
    let cursor = cursor.min(text.len());
    // Round the cursor down to a char boundary (defensive; editors pass
    // grapheme-boundary positions).
    let cursor = prev_char_boundary(text, cursor);
    let text_before_cursor = &text[..cursor];
    let segments = match options.segment {
        Some(f) => f(text_before_cursor),
        None => segment_text(text_before_cursor),
    };
    let is_atomic = options.is_atomic_segment;
    let mut segments: VecDeque<Segment> = segments.into();
    let mut new_cursor = cursor;

    // Skip trailing whitespace
    while let Some(last) = segments.back() {
        let seg = &last.segment;
        let atomic = is_atomic.map(|f| f(seg)).unwrap_or(false);
        if !atomic && seg.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
            new_cursor -= seg.len();
            segments.pop_back();
        } else {
            break;
        }
    }

    if segments.is_empty() {
        return new_cursor;
    }

    let last = segments.back().unwrap().clone();
    let atomic = is_atomic.map(|f| f(&last.segment)).unwrap_or(false);

    if atomic {
        // Skip one atomic segment.
        segments.pop_back();
        new_cursor -= last.segment.len();
    } else if last.is_word_like {
        segments.pop_back();
        // Skip inside one word-like segment, preserving punctuation boundaries.
        if segment_has_no_punctuation(&last.segment) {
            new_cursor -= last.segment.len();
        } else if let Some(last_match_byte) = last_index_of_punctuation(&last.segment) {
            let last_match_char_len = last.segment[last_match_byte..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            new_cursor -= last.segment.len() - (last_match_byte + last_match_char_len);
        } else {
            new_cursor -= last.segment.len();
        }
    } else {
        // Skip non-word non-whitespace run (punctuation) — the current
        // segment is included in the loop, mirroring upstream.
        while let Some(last) = segments.back() {
            let seg = &last.segment;
            let atomic = is_atomic.map(|f| f(seg)).unwrap_or(false);
            if atomic || last.is_word_like || seg.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
                break;
            }
            new_cursor -= seg.len();
            segments.pop_back();
        }
    }

    new_cursor
}

/// Find the cursor position after moving one word forward.
pub fn find_word_forward(text: &str, cursor: usize, options: &WordNavigationOptions) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    // Round the cursor up to a char boundary (defensive).
    let cursor = next_char_boundary(text, cursor);
    let text_after_cursor = &text[cursor..];
    let segments = match options.segment {
        Some(f) => f(text_after_cursor),
        None => segment_text(text_after_cursor),
    };
    let is_atomic = options.is_atomic_segment;
    let mut segments = segments.into_iter().peekable();
    let mut new_cursor = cursor;

    // Skip leading whitespace
    while let Some(seg) = segments.peek() {
        let atomic = is_atomic.map(|f| f(&seg.segment)).unwrap_or(false);
        if !atomic && seg.segment.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
            new_cursor += seg.segment.len();
            segments.next();
        } else {
            break;
        }
    }

    let mut next = segments.next();
    let Some(first) = next.as_ref() else {
        return new_cursor;
    };
    let atomic = is_atomic.map(|f| f(&first.segment)).unwrap_or(false);

    if atomic {
        // Skip one atomic segment.
        new_cursor += first.segment.len();
    } else if first.is_word_like {
        // Skip inside one word-like segment, preserving punctuation boundaries
        // (first punctuation char index, or full length).
        match first.segment.char_indices().find(|(_, c)| is_punctuation_char(*c)) {
            Some((idx, _)) => new_cursor += idx,
            None => new_cursor += first.segment.len(),
        }
    } else {
        // Skip non-word non-whitespace run (punctuation) — the current
        // segment is included in the loop, mirroring upstream.
        while let Some(seg) = next.as_ref() {
            let atomic = is_atomic.map(|f| f(&seg.segment)).unwrap_or(false);
            if atomic || seg.is_word_like || seg.segment.chars().next().map(|c| c.is_whitespace()).unwrap_or(false) {
                break;
            }
            new_cursor += seg.segment.len();
            next = segments.next();
        }
    }

    new_cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> WordNavigationOptions<'static> {
        WordNavigationOptions::default()
    }

    #[test]
    fn backward_basic_words() {
        let text = "hello world";
        assert_eq!(find_word_backward(text, 11, &opts()), 6);
        assert_eq!(find_word_backward(text, 6, &opts()), 0);
    }

    #[test]
    fn backward_dotted() {
        let text = "foo.bar";
        assert_eq!(find_word_backward(text, 7, &opts()), 4);
        assert_eq!(find_word_backward(text, 4, &opts()), 3);
        assert_eq!(find_word_backward(text, 3, &opts()), 0);
    }

    #[test]
    fn backward_colon() {
        let text = "foo:bar";
        assert_eq!(find_word_backward(text, 7, &opts()), 4);
        assert_eq!(find_word_backward(text, 4, &opts()), 3);
        assert_eq!(find_word_backward(text, 3, &opts()), 0);
    }

    #[test]
    fn backward_path() {
        let text = "path/to/file";
        assert_eq!(find_word_backward(text, 12, &opts()), 8);
        assert_eq!(find_word_backward(text, 8, &opts()), 7);
        // "/to" is one word-like segment with "/" as punctuation boundary
        assert_eq!(find_word_backward(text, 7, &opts()), 5);
        assert_eq!(find_word_backward(text, 5, &opts()), 4);
        assert_eq!(find_word_backward(text, 4, &opts()), 0);
    }

    #[test]
    fn backward_cjk_mixed() {
        let text = "你好世界 test";
        // Documented divergence: upstream uses ICU dictionary word
        // segmentation (e.g. grouping "你好" and "世界"), which cannot be
        // reproduced exactly. We group each contiguous CJK run as one
        // word-like segment, so one backward move skips the whole run.
        //
        // text.len() is 17 bytes ("你好世界" = 12 bytes + " " + "test").
        // "test" is one word-like segment: 17 -> 13.
        assert_eq!(find_word_backward(text, text.len(), &opts()), 13);
        // Backward again skips the trailing space and then the CJK run: 0.
        assert_eq!(find_word_backward(text, 13, &opts()), 0);
        assert_eq!(find_word_backward(text, 2, &opts()), 0);
    }

    #[test]
    fn backward_whitespace_boundaries() {
        let text = "  hello  ";
        assert_eq!(find_word_backward(text, 9, &opts()), 2);
        assert_eq!(find_word_backward(text, 2, &opts()), 0);
    }

    #[test]
    fn backward_punctuation_run() {
        let text = "foo...bar";
        assert_eq!(find_word_backward(text, 9, &opts()), 6);
        assert_eq!(find_word_backward(text, 6, &opts()), 3);
        assert_eq!(find_word_backward(text, 3, &opts()), 0);
    }

    #[test]
    fn backward_cursor_at_zero() {
        assert_eq!(find_word_backward("hello", 0, &opts()), 0);
    }

    #[test]
    fn forward_basic_words() {
        let text = "hello world";
        assert_eq!(find_word_forward(text, 0, &opts()), 5);
        assert_eq!(find_word_forward(text, 5, &opts()), 11);
    }

    #[test]
    fn forward_dotted() {
        let text = "foo.bar";
        assert_eq!(find_word_forward(text, 0, &opts()), 3);
        assert_eq!(find_word_forward(text, 3, &opts()), 4);
        assert_eq!(find_word_forward(text, 4, &opts()), 7);
    }

    #[test]
    fn forward_colon() {
        let text = "foo:bar";
        assert_eq!(find_word_forward(text, 0, &opts()), 3);
        assert_eq!(find_word_forward(text, 3, &opts()), 4);
        assert_eq!(find_word_forward(text, 4, &opts()), 7);
    }

    #[test]
    fn forward_path() {
        let text = "path/to/file";
        assert_eq!(find_word_forward(text, 0, &opts()), 4);
        assert_eq!(find_word_forward(text, 4, &opts()), 5);
        assert_eq!(find_word_forward(text, 5, &opts()), 7);
        assert_eq!(find_word_forward(text, 7, &opts()), 8);
        assert_eq!(find_word_forward(text, 8, &opts()), 12);
    }

    #[test]
    fn forward_cjk_walk_reaches_end() {
        let text = "你好世界 test";
        let mut pos = 0;
        while pos < text.len() {
            let next = find_word_forward(text, pos, &opts());
            if next == pos {
                break;
            }
            pos = next;
        }
        assert_eq!(pos, text.len());
    }

    #[test]
    fn forward_whitespace_boundaries() {
        let text = "  hello  ";
        assert_eq!(find_word_forward(text, 0, &opts()), 7);
        assert_eq!(find_word_forward(text, 7, &opts()), 9);
    }

    #[test]
    fn forward_punctuation_run() {
        let text = "foo...bar";
        assert_eq!(find_word_forward(text, 0, &opts()), 3);
        assert_eq!(find_word_forward(text, 3, &opts()), 6);
        assert_eq!(find_word_forward(text, 6, &opts()), 9);
    }

    #[test]
    fn forward_cursor_at_end() {
        assert_eq!(find_word_forward("hello", 5, &opts()), 5);
    }

    #[test]
    fn atomic_segments() {
        let marker = "[paste #1 +5 lines]";
        let _text = format!("hello {marker} world");
        let is_atomic = |s: &str| s == marker;
        let custom_segment = |input: &str| {
            // Marker-aware segmenter: absorb the whole paste marker as one
            // atomic segment (mirrors the editor's segmentWithMarkers).
            let mut result: Vec<Segment> = Vec::new();
            let mut pos = 0usize;
            while let Some(rel) = input[pos..].find(marker) {
                let start = pos + rel;
                if start > pos {
                    result.extend(segment_text(&input[pos..start]));
                }
                result.push(Segment { segment: marker.to_string(), index: start, is_word_like: true });
                pos = start + marker.len();
            }
            if pos < input.len() {
                result.extend(segment_text(&input[pos..]));
            }
            result
        };
        let opts = WordNavigationOptions {
            segment: Some(&custom_segment),
            is_atomic_segment: Some(&is_atomic),
        };
        // Backward from end skips "world" then stops at the marker boundary.
        let full = format!("hello {marker} world");
        // "world" is 5 chars after marker + space.
        assert_eq!(find_word_backward(&full, full.len(), &opts), 26);
        // Backward skips whitespace then the atomic marker as one unit.
        assert_eq!(find_word_backward(&full, 6 + marker.len(), &opts), 6);
        // Forward skips the atomic marker as one unit.
        assert_eq!(find_word_forward(&full, 6, &opts), 6 + marker.len());
    }
}


