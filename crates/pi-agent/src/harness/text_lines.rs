//! Pull-based UTF-8 line reader + line splitter — port of the 0.85.1
//! upstream `TextLine`/`TextLineReader` surface (`harness/types.ts`).
//!
//! `split_text_lines` preserves final-line termination: a trailing segment
//! without `\n` yields `terminated: false` so callers can discard a torn
//! final record, matching upstream exactly.

/// One UTF-8 line read from a text file.
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub text: String,
    /// Whether the line ended with `\n`; callers use this to discard a torn
    /// final record.
    pub terminated: bool,
}

/// Split text into lines preserving final-line termination.
pub fn split_text_lines(text: &str) -> Vec<TextLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut segments: Vec<&str> = text.split('\n').collect();
    // A trailing newline terminates the last line; the empty artifact after
    // it is not a line.
    let final_terminated = text.ends_with('\n');
    if final_terminated {
        segments.pop();
    }
    let mut lines: Vec<TextLine> = segments
        .into_iter()
        .map(|segment| TextLine {
            text: segment.to_string(),
            terminated: true,
        })
        .collect();
    if !final_terminated {
        if let Some(last) = lines.last_mut() {
            last.terminated = false;
        }
    }
    lines
}
