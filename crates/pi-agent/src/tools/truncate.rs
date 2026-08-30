//! Output truncation helpers — port of
//! `packages/agent/src/harness/utils/truncate.ts`.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truncation {
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub content: String,
}

/// UTF-8 safe byte length (port of `utf8Length` semantics used upstream).
pub fn utf8_len(text: &str) -> usize {
    text.len()
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Keep the leading complete lines of content within the configured limits.
/// Never returns partial lines.
pub fn truncate_head(content: &str) -> Truncation {
    truncate_head_with(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

pub fn truncate_head_with(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let total_bytes = utf8_len(content);
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncation {
            truncated: false,
            truncated_by: None,
            total_lines,
            output_lines: total_lines,
            output_bytes: total_bytes,
            content: content.to_string(),
        };
    }

    // The head tool never returns a partial line. If the first line alone is
    // too large, return no content and let the caller point the user at a
    // byte-oriented command, matching the upstream read-tool contract.
    if lines.first().is_some_and(|line| utf8_len(line) > max_bytes) {
        return Truncation {
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            output_lines: 0,
            output_bytes: 0,
            content: String::new(),
        };
    }

    let mut output_lines = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = utf8_len(line) + usize::from(index > 0);
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines.push(*line);
        output_bytes += line_bytes;
    }
    if output_lines.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let joined = output_lines.join("\n");
    Truncation {
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        output_lines: output_lines.len(),
        output_bytes: utf8_len(&joined),
        content: joined,
    }
}

/// Keep the trailing complete lines of content (bash tool behavior).
/// When a single long line exceeds the byte limit, keep the whole line but
/// report it as a partial-line edge case via `last_line_partial`.
pub fn truncate_tail(content: &str) -> (Truncation, bool, usize) {
    truncate_tail_with(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

pub fn truncate_tail_with(
    content: &str,
    max_lines: usize,
    max_bytes: usize,
) -> (Truncation, bool, usize) {
    let total_bytes = utf8_len(content);
    let all_lines = split_lines_for_counting(content);
    let total_lines = all_lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return (
            Truncation {
                truncated: false,
                truncated_by: None,
                total_lines,
                output_lines: total_lines,
                output_bytes: total_bytes,
                content: content.to_string(),
            },
            false,
            0,
        );
    }

    let mut selected = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut original_last_line_bytes = 0usize;
    for line in all_lines.iter().rev().take(max_lines) {
        let line_bytes = utf8_len(line) + usize::from(!selected.is_empty());
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if selected.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                original_last_line_bytes = utf8_len(line);
                output_bytes = utf8_len(&truncated_line);
                selected.push(truncated_line);
                last_line_partial = true;
            }
            break;
        }
        output_bytes += line_bytes;
        selected.push((*line).to_string());
    }
    selected.reverse();
    if selected.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let joined = selected.join("\n");
    (
        Truncation {
            truncated: true,
            truncated_by: Some(truncated_by),
            total_lines,
            output_lines: selected.len(),
            output_bytes: utf8_len(&joined),
            content: joined,
        },
        last_line_partial,
        if last_line_partial {
            original_last_line_bytes
        } else {
            0
        },
    )
}

fn truncate_string_to_bytes_from_end(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || text.len() <= max_bytes {
        return if max_bytes == 0 {
            String::new()
        } else {
            text.to_string()
        };
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && (text.as_bytes()[start] & 0xc0) == 0x80 {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn head_truncates_by_lines() {
        let content = (0..2002)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head(&content);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(t.output_lines, 2000);
    }

    #[test]
    fn head_truncates_by_bytes() {
        let content = "a".repeat(60 * 1024);
        let t = truncate_head(&content);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn no_truncation_for_small_content() {
        let t = truncate_head("hello\nworld");
        assert!(!t.truncated);
        assert_eq!(t.content, "hello\nworld");
    }

    #[test]
    fn head_rejects_a_single_line_over_the_byte_limit() {
        let t = truncate_head_with("ééé", 2000, 4);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(t.output_lines, 0);
        assert!(t.content.is_empty());
    }

    #[test]
    fn trailing_newline_is_not_counted_as_an_extra_line() {
        let t = truncate_head_with("one\ntwo\n", 2, 50);
        assert!(!t.truncated);
        assert_eq!(t.total_lines, 2);
        assert_eq!(t.output_bytes, 8);
    }

    #[test]
    fn tail_partial_line_reports_original_line_size() {
        let (t, partial, line_bytes) = truncate_tail_with("αβγδε", 2000, 5);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Bytes));
        assert!(partial);
        assert_eq!(line_bytes, "αβγδε".len());
        assert!(t.output_bytes <= 5);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let content = (0..2005)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (t, _, _) = truncate_tail(&content);
        assert!(t.truncated);
        assert_eq!(t.output_lines, 2000);
        assert!(t.content.starts_with("line 5"));
        assert!(t.content.ends_with("line 2004"));
    }
}
