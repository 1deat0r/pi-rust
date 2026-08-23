//! ANSI-aware text utilities — port of `packages/tui/src/utils.ts`.

/// Visible width of a string, ignoring ANSI escape sequences. Tab counts as 1.
pub fn visible_width(text: &str) -> usize {
    strip_ansi_codes(text).chars().count()
}

/// Strip ANSI SGR/CSI escape sequences (used for width math).
pub fn strip_ansi_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ ... final byte; OSC: ESC ] ... BEL/ST
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        let _ = chars.next(); // '\' of ST
                        break;
                    }
                }
            } else {
                let _ = chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Tokenize text into (visible_char_or_space, is_escape) units.
fn tokenize(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut seq = String::new();
            seq.push(c);
            if chars.peek() == Some(&'[') {
                seq.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    seq.push(c2);
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                seq.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    seq.push(c2);
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        if let Some(c3) = chars.next() {
                            seq.push(c3);
                        }
                        break;
                    }
                }
            } else if let Some(c2) = chars.next() {
                seq.push(c2);
            }
            out.push((seq, true));
            continue;
        }
        out.push((c.to_string(), false));
    }
    out
}

/// Wrap text to a width, preserving ANSI sequences (word-wrap + hard wrap).
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let tokens = tokenize(text);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    // Current word: tokens (escapes + chars) with their visible width.
    let mut word: Vec<(String, usize)> = Vec::new();
    let mut word_width = 0usize;

    let flush_word = |current: &mut String,
                      current_width: &mut usize,
                      word: &mut Vec<(String, usize)>,
                      word_width: &mut usize,
                      lines: &mut Vec<String>| {
        if word.is_empty() {
            return;
        }
        if *current_width + *word_width > width && *current_width > 0 {
            lines.push(std::mem::take(current));
            *current_width = 0;
        }
        if *word_width >= width {
            // Hard-break long words: prefer filling the current line first,
            // then split the word into width chunks.
            for (tok, w) in std::mem::take(word) {
                if w == 0 {
                    *current += &tok;
                    continue;
                }
                if *current_width == width {
                    lines.push(std::mem::take(current));
                    *current_width = 0;
                }
                let mut remaining = tok;
                while !remaining.is_empty() {
                    let take = (width - *current_width).max(1);
                    let cut: String = remaining.chars().take(take).collect();
                    *current += &cut;
                    *current_width += strip_ansi_codes(&cut).chars().count();
                    if remaining.chars().count() > cut.chars().count() {
                        lines.push(std::mem::take(current));
                        *current_width = 0;
                    }
                    remaining = remaining.chars().skip(cut.chars().count()).collect();
                }
            }
            *word_width = 0;
            return;
        }
        for (tok, w) in word.drain(..) {
            *current += &tok;
            *current_width += w;
        }
        *word_width = 0;
    };

    for (tok, is_escape) in tokens {
        if is_escape {
            word.push((tok, 0));
            continue;
        }
        let c = tok.chars().next().unwrap();
        if c == ' ' || c == '\n' {
            flush_word(
                &mut current,
                &mut current_width,
                &mut word,
                &mut word_width,
                &mut lines,
            );
            if c == '\n' {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            } else if current_width < width {
                current.push(' ');
                current_width += 1;
            }
            continue;
        }
        let w = if c == '\t' { 3 } else { 1 };
        word.push((tok, w));
        word_width += w;
    }
    flush_word(
        &mut current,
        &mut current_width,
        &mut word,
        &mut word_width,
        &mut lines,
    );
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Slice a string to a visible column count (ANSI-aware).
pub fn slice_with_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut seen = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy the escape sequence fully.
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                out.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        if let Some(c3) = chars.next() {
                            out.push(c3);
                        }
                        break;
                    }
                }
            } else if let Some(c2) = chars.next() {
                out.push(c2);
            }
            continue;
        }
        if seen >= width {
            break;
        }
        out.push(c);
        seen += 1;
    }
    out
}

/// Truncate text to a visible width with an optional ellipsis (upstream
/// `truncateToWidth`, ASCII/plain-text subset with ANSI-aware widths).
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str) -> String {
    if max_width == 0 {
        return String::new();
    }
    let text_width = visible_width(text);
    if text_width <= max_width {
        return text.to_string();
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let clipped = slice_with_width(ellipsis, max_width);
        return clipped;
    }
    let target = max_width - ellipsis_width;
    let sliced = slice_with_width(text, target);
    format!("{sliced}{ellipsis}")
}

/// Upstream `sliceByColumn` — slice by visible columns (ANSI-aware).
pub fn slice_by_column(line: &str, start_col: usize, length: usize) -> String {
    let prefix = slice_with_width(line, start_col);
    let _ = prefix;
    // Skip start_col visible columns then take `length`.
    let mut out = String::new();
    let mut seen = 0usize;
    let mut chars = line.chars().peekable();
    let mut skipped = 0usize;
    let mut started = false;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut seq = String::new();
            seq.push(c);
            if chars.peek() == Some(&'[') {
                seq.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    seq.push(c2);
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                seq.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    seq.push(c2);
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        if let Some(c3) = chars.next() {
                            seq.push(c3);
                        }
                        break;
                    }
                }
            } else if let Some(c2) = chars.next() {
                seq.push(c2);
            }
            if started {
                out.push_str(&seq);
            }
            continue;
        }
        if !started {
            if skipped >= start_col {
                started = true;
            } else {
                skipped += 1;
                continue;
            }
        }
        if seen >= length {
            break;
        }
        out.push(c);
        seen += 1;
    }
    out
}

/// Apply a background color function to a line, padding to full width.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let visible = visible_width(line);
    let padded = if visible < width {
        format!("{}{}", line, " ".repeat(width - visible))
    } else {
        line.to_string()
    };
    bg(&padded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_strips_ansi() {
        assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(visible_width("plain"), 5);
    }

    #[test]
    fn wrap_basic() {
        let lines = wrap_text_with_ansi("hello world foo", 10);
        assert_eq!(lines, vec!["hello ", "world foo"]);
    }

    #[test]
    fn wrap_hard_breaks_long_words() {
        let lines = wrap_text_with_ansi("abcdefghijklmnop", 6);
        assert_eq!(lines[0], "abcdef");
        assert_eq!(lines[1], "ghijkl");
        assert_eq!(lines[2], "mnop");
    }

    #[test]
    fn slice_respects_width() {
        assert_eq!(slice_with_width("hello world", 5), "hello");
        assert_eq!(slice_with_width("héllo", 3), "hél");
    }

    #[test]
    fn slice_keeps_ansi() {
        assert_eq!(
            slice_with_width("\x1b[31mred\x1b[0m", 3),
            "\x1b[31mred\x1b[0m"
        );
    }

    #[test]
    fn truncate_to_width_basic() {
        assert_eq!(truncate_to_width("hello world", 5, "..."), "he...");
        assert_eq!(truncate_to_width("hello", 8, "..."), "hello");
        assert_eq!(truncate_to_width("hello world", 8, "..."), "hello...");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    #[test]
    fn probe_hard_break() {
        let lines = wrap_text_with_ansi("abcdefghijklmnop", 6);
        eprintln!("PROBE lines: {:?}", lines);
        assert_eq!(lines[0], "abcdef");
        assert_eq!(lines[1], "ghijkl");
    }
}
