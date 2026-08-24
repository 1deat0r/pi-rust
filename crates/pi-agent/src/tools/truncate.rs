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
        format!("{}KB", bytes / 1024)
    } else {
        format!("{}MB", bytes / (1024 * 1024))
    }
}

/// Keep the leading complete lines of content within the configured limits.
/// Never returns partial lines.
pub fn truncate_head(content: &str) -> Truncation {
    truncate_head_with(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

pub fn truncate_head_with(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let total_lines = content.lines().count();
    let mut lines: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by = None;
    for line in content.lines().take(max_lines + 1) {
        if lines.len() >= max_lines {
            truncated_by = Some(TruncatedBy::Lines);
            break;
        }
        bytes += utf8_len(line) + 1; // + newline
        if bytes > max_bytes {
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }
        lines.push(line);
    }
    let joined = lines.join("\n");
    Truncation {
        truncated: truncated_by.is_some(),
        truncated_by,
        total_lines,
        output_lines: lines.len(),
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
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();
    let mut start = 0usize;
    let mut bytes = 0usize;
    let mut truncated_by = None;
    // Walk from the end, accumulating full lines.
    for (idx, line) in all_lines.iter().enumerate().rev() {
        bytes += utf8_len(line) + 1;
        if bytes > max_bytes || (all_lines.len() - idx) > max_lines {
            truncated_by = Some(if bytes > max_bytes {
                TruncatedBy::Bytes
            } else {
                TruncatedBy::Lines
            });
            break;
        }
        start = idx;
    }
    let selected: Vec<&str> = all_lines[start..].to_vec();
    let last_line_partial = truncated_by == Some(TruncatedBy::Bytes)
        && selected.len() == 1
        && selected[0].len() > max_bytes;
    let joined = selected.join("\n");
    (
        Truncation {
            truncated: truncated_by.is_some(),
            truncated_by,
            total_lines,
            output_lines: selected.len(),
            output_bytes: utf8_len(&joined),
            content: joined,
        },
        last_line_partial,
        if last_line_partial {
            utf8_len(selected[0])
        } else {
            0
        },
    )
}

#[cfg(test)]
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
