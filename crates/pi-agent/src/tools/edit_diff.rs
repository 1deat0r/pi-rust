//! Edit diff machinery — port of
//! `packages/agent/src/harness/tools/edit-diff.ts` (detectLineEnding,
//! normalizeToLF, normalizeForFuzzyMatch, fuzzyFindText, countOccurrences,
//! applyEditsToNormalizedContent, generateDiffString, generateUnifiedPatch).
//! Pure logic; no fs.
//!
//! NOTE on index units: upstream computes match indices in UTF-16 code units
//! (JS `indexOf`/`substring`). The Rust port uses byte offsets consistently
//! (all indices derive from `str::find` over the same string), so operations
//! are internally coherent. Byte-exact positional parity with a JS oracle is
//! only asserted for ASCII edits in tests.

use similar::{capture_diff_slices, Algorithm, DiffOp};
use unicode_normalization::UnicodeNormalization;

// ---------------------------------------------------------------------------
// Line endings + normalization
// ---------------------------------------------------------------------------

pub fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(c), Some(l)) if c < l => "\r\n",
        _ => "\n",
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

const SMART_SINGLE_QUOTES: [char; 4] = ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];
const SMART_DOUBLE_QUOTES: [char; 4] = ['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'];
const DASHES: [char; 7] = [
    '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
];
const SPECIAL_SPACES: [char; 13] = [
    '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
    '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
];

/// Upstream `normalizeForFuzzyMatch`: NFKC, strip trailing whitespace per
/// line, smart quotes → ASCII, dashes/hyphens → '-', special spaces → ' '.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc = text.nfkc().collect::<String>();
    let lines: Vec<&str> = nfkc.split('\n').map(|l| l.trim_end()).collect();
    let joined = lines.join("\n");
    let mut out = String::with_capacity(joined.len());
    for c in joined.chars() {
        if SMART_SINGLE_QUOTES.contains(&c) {
            out.push('\'');
        } else if SMART_DOUBLE_QUOTES.contains(&c) {
            out.push('"');
        } else if DASHES.contains(&c) {
            out.push('-');
        } else if SPECIAL_SPACES.contains(&c) {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// `[^\n]*\n|[^\n]+` — lines including their trailing newline.
pub fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            out.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
}

pub fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan { start: offset, end: offset + line.len() };
            offset = span.end;
            span
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fuzzy matching
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatchResult {
    pub found: bool,
    /// Byte index where the match starts (in the content passed in).
    pub index: usize,
    /// Byte length of the matched text.
    pub match_length: usize,
    /// Whether fuzzy matching was used (false = exact match).
    pub used_fuzzy_match: bool,
}

/// Find oldText in content, trying exact match first, then fuzzy match
/// (against `normalizeForFuzzyMatch` views of both sides).
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
        };
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(index) => FuzzyMatchResult {
            found: true,
            index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
        },
        None => FuzzyMatchResult { found: false, index: 0, match_length: 0, used_fuzzy_match: false },
    }
}

/// Upstream `countOccurrences`: occurrences of oldText in normalized space.
pub fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.split(&fuzzy_old_text).count().saturating_sub(1)
}

// ---------------------------------------------------------------------------
// Applying edits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

#[derive(Debug, Clone)]
struct TextReplacement {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

// ---------------------------------------------------------------------------
// Error messages (exact port; tests assert on them)
// ---------------------------------------------------------------------------

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(path: &str, edit_index: usize, total_edits: usize, occurrences: usize) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

fn overlap_error(path: &str, previous: usize, current: usize) -> String {
    format!(
        "edits[{previous}] and edits[{current}] overlap in {path}. Merge them into one edit or target disjoint regions."
    )
}

// ---------------------------------------------------------------------------
// Replacement application
// ---------------------------------------------------------------------------

fn get_replacement_line_range(lines: &[LineSpan], replacement: &TextReplacement) -> (usize, usize) {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = Some(i);
            break;
        }
    }
    let Some(start_line) = start_line else {
        panic!("Replacement range is outside the base content.");
    };

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        panic!("Replacement range is outside the base content.");
    }

    (start_line, end_line + 1)
}

/// Apply replacements to `content` in reverse order so offsets stay stable.
fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for i in (0..replacements.len()).rev() {
        let replacement = &replacements[i];
        let match_index = replacement.match_index - offset;
        let head = &result[..match_index];
        let tail = &result[match_index + replacement.match_length..];
        result = format!("{head}{}{tail}", replacement.new_text);
    }
    result
}

/// Apply replacements matched against `base_content` to `original_content`
/// while preserving unchanged line blocks from the original (used by the
/// fuzzy path so duplicates cannot be misaligned and untouched bytes stay).
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> String {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        panic!("Cannot preserve unchanged lines because the base content has a different line count.");
    }

    // Group replacements by the base lines they touch.
    let mut sorted: Vec<&TextReplacement> = replacements.iter().collect();
    sorted.sort_by_key(|r| r.match_index);
    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, replacement);
        if let Some(current) = groups.last_mut() {
            if start_line < current.1 {
                current.1 = current.1.max(end_line);
                current.2.push(replacement.clone());
                continue;
            }
        }
        groups.push((start_line, end_line, vec![replacement.clone()]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (group_start, group_end, group_replacements) in groups {
        result.push_str(&original_lines[original_line_index..group_start].join(""));
        let group_start_offset = base_lines[group_start].start;
        let group_end_offset = base_lines[group_end - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &group_replacements,
            group_start_offset,
        ));
        original_line_index = group_end;
    }
    result.push_str(&original_lines[original_line_index..].join(""));
    result
}

/// Apply one or more exact-text replacements to LF-normalized content (the
/// upstream `applyEditsToNormalizedContent`).
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, i, normalized_edits.len()));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|e| fuzzy_find_text(normalized_content, &e.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content =
        if used_fuzzy_match { normalize_for_fuzzy_match(normalized_content) } else { normalized_content.to_string() };

    let mut matched_edits: Vec<TextReplacement> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(not_found_error(path, i, normalized_edits.len()));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(path, i, normalized_edits.len(), occurrences));
        }
        matched_edits.push(TextReplacement {
            edit_index: i,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|m| m.match_index);
    for i in 1..matched_edits.len() {
        let previous = &matched_edits[i - 1];
        let current = &matched_edits[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(overlap_error(path, previous.edit_index, current.edit_index));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )
    } else {
        apply_replacements(&replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult { base_content, new_content })
}


// ---------------------------------------------------------------------------
// Display diff + unified patch
// ---------------------------------------------------------------------------

/// Strip UTF-8 BOM if present; return BOM and the text without it.
pub fn strip_bom(content: &str) -> (String, String) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), rest.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

#[derive(Debug, Clone)]
enum DiffLine {
    Context(String),
    Removed(String),
    Added(String),
}

/// npm-diffLines-compatible content split: drop the phantom trailing empty
/// element when content ends with a newline ("a\nb\n" -> ["a", "b"]).
fn split_content_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Line-oriented diff parts between old and new content (order preserved).
/// A `Replace` op (Myers for lines merges adjacent delete/insert) is flattened
/// into Removed then Added lines.
fn line_diff_parts(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = split_content_lines(old);
    let new_lines: Vec<&str> = split_content_lines(new);
    let ops = capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines);
    let mut parts = Vec::new();
    for op in ops {
        match &op {
            DiffOp::Equal { old_index, len, .. } => {
                for i in 0..*len {
                    parts.push(DiffLine::Context(old_lines[*old_index + i].to_string()));
                }
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                for i in 0..*old_len {
                    parts.push(DiffLine::Removed(old_lines[*old_index + i].to_string()));
                }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                for i in 0..*new_len {
                    parts.push(DiffLine::Added(new_lines[*new_index + i].to_string()));
                }
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                for i in 0..*old_len {
                    parts.push(DiffLine::Removed(old_lines[*old_index + i].to_string()));
                }
                for i in 0..*new_len {
                    parts.push(DiffLine::Added(new_lines[*new_index + i].to_string()));
                }
            }
        }
    }
    parts
}

fn pad(width: usize, n: usize) -> String {
    format!("{n:>width$}")
}

/// Upstream `generateDiffString`: display-oriented diff with line numbers
/// and context lines. Returns the diff and the first changed line
/// (in the new file).
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let parts = line_diff_parts(old_content, new_content);
    let old_line_count = split_content_lines(old_content).len();
    let new_line_count = split_content_lines(new_content).len();
    let line_num_width = old_line_count.max(new_line_count).to_string().len();

    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;
    let mut output: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < parts.len() {
        if matches!(parts[i], DiffLine::Added(_) | DiffLine::Removed(_)) {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            while i < parts.len() && matches!(parts[i], DiffLine::Added(_) | DiffLine::Removed(_)) {
                match &parts[i] {
                    DiffLine::Added(line) => {
                        output.push(format!("+{} {}", pad(line_num_width, new_line_num), line));
                        new_line_num += 1;
                    }
                    DiffLine::Removed(line) => {
                        output.push(format!("-{} {}", pad(line_num_width, old_line_num), line));
                        old_line_num += 1;
                    }
                    _ => unreachable!(),
                }
                i += 1;
            }
            last_was_change = true;
            continue;
        }

        // Context run: [context_start, i)
        let context_start = i;
        while i < parts.len() && matches!(parts[i], DiffLine::Context(_)) {
            i += 1;
        }
        let raw_len = i - context_start;
        let has_leading_change = last_was_change;
        let has_trailing_change =
            i < parts.len() && matches!(parts[i], DiffLine::Added(_) | DiffLine::Removed(_));

        let emit_context = |parts: &[DiffLine],
                            range: std::ops::Range<usize>,
                            output: &mut Vec<String>,
                            o_line: usize,
                            n_line: usize|
         -> (usize, usize) {
            let mut o = o_line;
            let mut n = n_line;
            for part in &parts[range] {
                if let DiffLine::Context(text) = part {
                    output.push(format!(" {} {}", pad(line_num_width, o), text));
                }
                o += 1;
                n += 1;
            }
            (o, n)
        };

        if has_leading_change && has_trailing_change {
            if raw_len <= context_lines * 2 {
                let (o, n) = emit_context(&parts, context_start..context_start + raw_len, &mut output, old_line_num, new_line_num);
                old_line_num = o;
                new_line_num = n;
            } else {
                let (o, n) = emit_context(&parts, context_start..context_start + context_lines, &mut output, old_line_num, new_line_num);
                old_line_num = o;
                new_line_num = n;
                output.push(format!(" {} ...", " ".repeat(line_num_width)));
                let skipped = raw_len - context_lines * 2;
                old_line_num += skipped;
                new_line_num += skipped;
                let (o, n) = emit_context(
                    &parts,
                    context_start + raw_len - context_lines..context_start + raw_len,
                    &mut output,
                    old_line_num,
                    new_line_num,
                );
                old_line_num = o;
                new_line_num = n;
            }
        } else if has_leading_change {
            let shown = raw_len.min(context_lines);
            let (o, n) = emit_context(&parts, context_start..context_start + shown, &mut output, old_line_num, new_line_num);
            old_line_num = o;
            new_line_num = n;
            let skipped = raw_len - shown;
            if skipped > 0 {
                output.push(format!(" {} ...", " ".repeat(line_num_width)));
                old_line_num += skipped;
                new_line_num += skipped;
            }
        } else if has_trailing_change {
            let skipped = raw_len.saturating_sub(context_lines);
            if skipped > 0 {
                output.push(format!(" {} ...", " ".repeat(line_num_width)));
                old_line_num += skipped;
                new_line_num += skipped;
            }
            let (o, n) = emit_context(&parts, context_start + skipped..context_start + raw_len, &mut output, old_line_num, new_line_num);
            old_line_num = o;
            new_line_num = n;
        } else {
            old_line_num += raw_len;
            new_line_num += raw_len;
        }
        last_was_change = false;
    }

    (output.join("\n"), first_changed_line)
}

/// Generate a standard unified patch (upstream `createTwoFilesPatch` with
/// FILE_HEADERS_ONLY — no timestamps) with the given context lines.
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let parts = line_diff_parts(old_content, new_content);

    // Annotate with 0-based old/new line positions.
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut annotated: Vec<(u8, String, usize, usize)> = Vec::new();
    for part in &parts {
        match part {
            DiffLine::Context(t) => {
                annotated.push((b' ', t.clone(), old_line, new_line));
                old_line += 1;
                new_line += 1;
            }
            DiffLine::Removed(t) => {
                annotated.push((b'-', t.clone(), old_line, new_line));
                old_line += 1;
            }
            DiffLine::Added(t) => {
                annotated.push((b'+', t.clone(), old_line, new_line));
                new_line += 1;
            }
        }
    }

    let change_positions: Vec<usize> = annotated
        .iter()
        .enumerate()
        .filter(|(_, (tag, _, _, _))| *tag != b' ')
        .map(|(i, _)| i)
        .collect();

    // Fold hunks: merge change groups separated by <= 2*context unchanged lines.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut idx = 0usize;
    while idx < change_positions.len() {
        let mut first = change_positions[idx];
        let mut last = first;
        while idx + 1 < change_positions.len() {
            let next = change_positions[idx + 1];
            if next - last - 1 <= context_lines * 2 {
                last = next;
                idx += 1;
            } else {
                break;
            }
        }
        first = first.saturating_sub(context_lines);
        let end = (last + 1 + context_lines).min(annotated.len());
        hunks.push((first, end));
        idx += 1;
    }

    if hunks.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ {path}\n"));
    for (start, end) in hunks {
        let mut o_count = 0usize;
        let mut n_count = 0usize;
        for (tag, _, _, _) in &annotated[start..end] {
            match tag {
                b' ' => {
                    o_count += 1;
                    n_count += 1;
                }
                b'-' => o_count += 1,
                b'+' => n_count += 1,
                _ => {}
            }
        }
        let o_start = annotated[start].2 + 1;
        let n_start = annotated[start].3 + 1;
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            hunk_range(o_start, o_count),
            hunk_range(n_start, n_count)
        ));
        for (tag, text, _, _) in &annotated[start..end] {
            out.push_str(&format!("{}{}\n", *tag as char, text));
        }
    }
    out
}

fn hunk_range(start: usize, count: usize) -> String {
    if count == 0 {
        format!("{start},0")
    } else if count == 1 {
        format!("{start}")
    } else {
        format!("{start},{count}")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit { old_text: old.to_string(), new_text: new.to_string() }
    }

    // ---- basic helpers -----------------------------------------------------

    #[test]
    fn detect_line_ending_crlf_vs_lf() {
        assert_eq!(detect_line_ending("a\nb\n"), "\n");
        assert_eq!(detect_line_ending("a\r\nb\r\n"), "\r\n");
        assert_eq!(detect_line_ending("no newlines"), "\n");
    }

    #[test]
    fn normalize_and_restore_line_endings() {
        assert_eq!(normalize_to_lf("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(restore_line_endings("a\nb\n", "\r\n"), "a\r\nb\r\n");
        assert_eq!(restore_line_endings("a\nb\n", "\n"), "a\nb\n");
    }

    #[test]
    fn fuzzy_normalization_transforms() {
        let n = normalize_for_fuzzy_match(" const = 'don\u{2019}t'\t ");
        assert_eq!(n, " const = 'don't'");
        let n = normalize_for_fuzzy_match("a\u{2013}b\u{00A0}c");
        assert_eq!(n, "a-b c");
        let fullwidth = normalize_for_fuzzy_match("\u{FF21}");
        assert_eq!(fullwidth, "A");
    }

    #[test]
    fn fuzzy_find_exact_first() {
        let r = fuzzy_find_text("alpha beta gamma", "beta");
        assert!(r.found);
        assert!(!r.used_fuzzy_match);
        assert_eq!(r.index, 6);
    }

    #[test]
    fn fuzzy_find_falls_back_for_smart_quote_and_whitespace() {
        let r = fuzzy_find_text("it\u{2019}s here\n", "it's here");
        assert!(r.found, "smart quote should fuzzy-match");
        assert!(r.used_fuzzy_match);
        let r2 = fuzzy_find_text("line with trailing  \nnext", "line with trailing\nnext");
        assert!(r2.found, "trailing whitespace should fuzzy-match");
        assert!(r2.used_fuzzy_match);
    }

    #[test]
    fn fuzzy_find_missing() {
        let r = fuzzy_find_text("alpha beta", "omega");
        assert!(!r.found);
    }

    #[test]
    fn count_occurrences_fuzzy() {
        assert_eq!(count_occurrences("foo foo foo", "foo"), 3);
        assert_eq!(count_occurrences("foo foo", "foo"), 2);
        assert_eq!(count_occurrences("foo", "bar"), 0);
    }

    // ---- apply_edits_to_normalized_content ----------------------------------

    #[test]
    fn applies_disjoint_exact_edits() {
        let result = apply_edits_to_normalized_content(
            "alpha\nbeta\ngamma\ndelta\n",
            &[edit("alpha\n", "ALPHA\n"), edit("gamma\n", "GAMMA\n")],
            "edit.txt",
        )
        .unwrap();
        assert_eq!(result.base_content, "alpha\nbeta\ngamma\ndelta\n");
        assert_eq!(result.new_content, "ALPHA\nbeta\nGAMMA\ndelta\n");
    }

    #[test]
    fn rejects_overlapping_edits() {
        let err = apply_edits_to_normalized_content(
            "one\ntwo\nthree\n",
            &[edit("one\ntwo\n", "ONE\nTWO\n"), edit("two\nthree\n", "TWO\nTHREE\n")],
            "edit.txt",
        )
        .unwrap_err();
        assert!(err.contains("overlap"), "got: {err}");
    }

    #[test]
    fn rejects_missing_and_duplicate() {
        let err = apply_edits_to_normalized_content("alpha beta gamma", &[edit("bar", "baz")], "edit.txt")
            .unwrap_err();
        assert!(err.contains("Could not find the exact text"), "got: {err}");

        let err = apply_edits_to_normalized_content("foo foo foo", &[edit("foo", "bar")], "edit.txt")
            .unwrap_err();
        assert!(err.contains("Found 3 occurrences"), "got: {err}");
    }

    #[test]
    fn rejects_empty_old_text() {
        let err = apply_edits_to_normalized_content("abc", &[edit("", "x")], "f").unwrap_err();
        assert!(err.contains("oldText must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_no_change() {
        let err = apply_edits_to_normalized_content("abc", &[edit("abc", "abc")], "f").unwrap_err();
        assert!(err.contains("No changes made"), "got: {err}");
    }

    #[test]
    fn fuzzy_edit_preserves_unchanged_lines_bytes() {
        // Curly apostrophe in the file; ASCII apostrophe in the edit target.
        let content = "fn main() {\n    println!(\"it\u{2019}s fine\");\n    other();\n}\n";
        let result =
            apply_edits_to_normalized_content(content, &[edit("it's fine", "it is fine")], "main.rs")
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            result.new_content,
            "fn main() {\n    println!(\"it is fine\");\n    other();\n}\n"
        );
    }

    #[test]
    fn multi_fuzzy_edit_disjoint_and_preserved() {
        let content = "fn main() {\n    one \"\u{201C}x\u{201D}\"\n    two\n    three \"\u{201C}y\u{201D}\"\n}\n";
        let result = apply_edits_to_normalized_content(
            content,
            &[edit("\"x\"", "\"X\""), edit("\"y\"", "\"Y\"")],
            "f.rs",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        // Fuzzy path rewrites each touched line from the normalized base, so
        // the curly quotes on *touched* lines become straight quotes; the
        // untouched "two" line keeps its original bytes.
        assert!(result.new_content.contains("one \"\"X\"\""), "got: {:?}", result.new_content);
        assert!(result.new_content.contains("three \"\"Y\"\""), "got: {:?}", result.new_content);
        assert!(result.new_content.contains("    two"), "got: {:?}", result.new_content);
    }

    #[test]
    fn strips_bom() {
        let (bom, text) = strip_bom("\u{FEFF}abc");
        assert_eq!(bom, "\u{FEFF}");
        assert_eq!(text, "abc");
        let (bom, text) = strip_bom("abc");
        assert_eq!(bom, "");
        assert_eq!(text, "abc");
    }

    // ---- generate_diff_string ----------------------------------------------

    #[test]
    fn diff_string_marks_changes_and_first_line() {
        let (diff, first) = generate_diff_string(
            "alpha\nbeta\ngamma\ndelta\n",
            "ALPHA\nbeta\nGAMMA\ndelta\n",
            4,
        );
        assert!(diff.contains("+1 ALPHA"), "got: {diff}");
        assert!(diff.contains("-1 alpha"), "got: {diff}");
        assert!(diff.contains("+3 GAMMA"), "got: {diff}");
        assert_eq!(first, Some(1));
    }

    #[test]
    fn diff_string_contexts_are_numbered() {
        let (diff, _) = generate_diff_string("one\ntwo\nthree\n", "one\nTWO\nthree\n", 4);
        assert!(diff.contains(" 1 one"), "got: {diff}");
        assert!(diff.contains(" 3 three"), "got: {diff}");
    }

    // ---- generate_unified_patch ---------------------------------------------

    #[test]
    fn unified_patch_headers_and_hunks() {
        let patch = generate_unified_patch(
            "edit.txt",
            "alpha\nbeta\ngamma\ndelta\n",
            "ALPHA\nbeta\nGAMMA\ndelta\n",
            4,
        );
        assert!(patch.starts_with("--- edit.txt\n+++ edit.txt\n"), "got: {patch}");
        assert!(patch.contains("@@ -1,4 +1,4 @@"), "got: {patch}");
        assert!(patch.contains("-alpha"), "got: {patch}");
        assert!(patch.contains("+ALPHA"), "got: {patch}");
    }

    #[test]
    fn unified_patch_applies_back_to_original() {
        let original = "alpha\nbeta\ngamma\ndelta\n";
        let expected = "ALPHA\nbeta\nGAMMA\ndelta\n";
        let patch = generate_unified_patch("edit.txt", original, expected, 4);
        let applied = apply_unified_patch_for_test(original, &patch);
        assert_eq!(applied, expected, "patch was:\n{patch}");
    }

    #[test]
    fn unified_patch_single_line_change() {
        let original = "one\ntwo\nthree\n";
        let expected = "one\nTWO\nthree\n";
        let patch = generate_unified_patch("f.txt", original, expected, 4);
        let applied = apply_unified_patch_for_test(original, &patch);
        assert_eq!(applied, expected, "patch was:\n{patch}");
    }

    /// Minimal unified-diff applier used only to verify our own patches.
    fn apply_unified_patch_for_test(original: &str, patch: &str) -> String {
        struct Hunk {
            o_start: usize,
            lines: Vec<(char, String)>, // ' ', '-', '+'
        }
        let mut hunks: Vec<Hunk> = Vec::new();
        for line in patch.lines() {
            if let Some(rest) = line.strip_prefix("@@ ") {
                let mut parts = rest.split_whitespace();
                let o = parts.next().unwrap().strip_prefix('-').unwrap().to_string();
                let _n = parts.next().unwrap().strip_prefix('+').unwrap().to_string();
                let o_start = o.split(',').next().unwrap().parse::<usize>().unwrap();
                hunks.push(Hunk { o_start, lines: Vec::new() });
                continue;
            }
            if line == "--- f.txt" || line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            if let Some(h) = hunks.last_mut() {
                if let Some(c) = line.chars().next() {
                    if c == ' ' || c == '-' || c == '+' {
                        h.lines.push((c, line[c.len_utf8()..].to_string()));
                    }
                }
            }
        }

        let orig_lines: Vec<String> = original.split('\n').map(|s| s.to_string()).collect();
        let mut result: Vec<String> = Vec::new();
        let mut o_idx = 0usize;
        for hunk in &hunks {
            let target_old = hunk.o_start.saturating_sub(1);
            while o_idx < target_old && o_idx < orig_lines.len() {
                result.push(orig_lines[o_idx].clone());
                o_idx += 1;
            }
            for (tag, text) in &hunk.lines {
                match tag {
                    ' ' => {
                        result.push(text.clone());
                        o_idx += 1;
                    }
                    '-' => {
                        o_idx += 1;
                    }
                    '+' => {
                        result.push(text.clone());
                    }
                    _ => {}
                }
            }
        }
        while o_idx < orig_lines.len() {
            result.push(orig_lines[o_idx].clone());
            o_idx += 1;
        }
        result.join("\n")
    }
}
