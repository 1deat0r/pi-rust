//! Markdown renderer — port of `packages/tui/src/components/markdown.ts`.
//!
//! Parses a Markdown subset modeled on `marked`'s token stream (headings,
//! paragraphs, fenced code, blockquotes, nested/task lists, tables, hr,
//! inline styles, links, strikethrough, LaTeX math blocks/inline) and renders
//! it as styled terminal lines.
//!
//! Documented divergence: the upstream parser is `marked`; this port embeds
//! a deterministic parser for the token shapes the transcripts use. The
//! remaining rendering model (theme application, wrapping, list/table layout,
//! LaTeX integration) follows the upstream class.

use crate::latex::render_latex;
use crate::terminal_image::{get_capabilities, hyperlink, is_image_line};
use crate::tui::Component;
use crate::utils::{apply_background_to_line, visible_width, wrap_text_with_ansi};

/// A text-style function: input text -> styled text.
pub type StyleFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Default text styling applied to all markdown text.
#[derive(Default)]
pub struct DefaultTextStyle {
    pub color: Option<StyleFn>,
    pub bg_color: Option<StyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

impl std::fmt::Debug for DefaultTextStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultTextStyle")
            .field("bold", &self.bold)
            .field("italic", &self.italic)
            .finish()
    }
}

/// A markdown-transform function (source, available width) -> source.
pub type TransformFn = Box<dyn Fn(&str, usize) -> String + Send + Sync>;
/// A code-highlight function (code, lang) -> styled lines.
pub type HighlightFn = Box<dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync>;

/// Theme functions for markdown elements.
pub struct MarkdownTheme {
    pub heading: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub link: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub link_url: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub code: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub code_block: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub code_block_border: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub quote: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub quote_border: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub hr: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub list_bullet: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub bold: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub italic: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub strikethrough: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub underline: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub highlight_code: Option<HighlightFn>,
    pub code_block_indent: Option<String>,
}

impl std::fmt::Debug for MarkdownTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkdownTheme").finish()
    }
}

/// Plain theme (identity functions) for tests.
pub fn plain_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(|s| s.to_string()),
        link: Box::new(|s| s.to_string()),
        link_url: Box::new(|s| s.to_string()),
        code: Box::new(|s| s.to_string()),
        code_block: Box::new(|s| s.to_string()),
        code_block_border: Box::new(|s| s.to_string()),
        quote: Box::new(|s| s.to_string()),
        quote_border: Box::new(|s| s.to_string()),
        hr: Box::new(|s| s.to_string()),
        list_bullet: Box::new(|s| s.to_string()),
        bold: Box::new(|s| s.to_string()),
        italic: Box::new(|s| s.to_string()),
        strikethrough: Box::new(|s| s.to_string()),
        underline: Box::new(|s| s.to_string()),
        highlight_code: None,
        code_block_indent: Some("  ".to_string()),
    }
}

pub struct MarkdownOptions {
    pub preserve_ordered_list_markers: bool,
    pub preserve_backslash_escapes: bool,
    pub transform: Option<TransformFn>,
    pub render_latex: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            transform: None,
            render_latex: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown parsing (marked-shaped subset)
// ---------------------------------------------------------------------------

/// Inline token.
#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Escape(String),
    Strong(Vec<Inline>),
    Em(Vec<Inline>),
    Codespan(String),
    Link {
        text: Vec<Inline>,
        href: String,
        raw_text: String,
    },
    Br,
    Del(Vec<Inline>),
    Html(String),
    Latex {
        text: String,
        raw: String,
        pending: bool,
    },
}

/// Block token.
#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: usize,
        tokens: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Text(Vec<Inline>),
    LatexBlock {
        text: String,
        raw: String,
        pending: bool,
    },
    Code {
        lang: String,
        text: String,
        raw: String,
    },
    List(ListBlock),
    Table(TableBlock),
    Blockquote(Vec<Block>),
    Hr,
    Html(String),
    Space,
}

#[derive(Debug, Clone)]
pub struct ListBlock {
    pub ordered: bool,
    pub start: usize,
    pub loose: bool,
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone)]
pub struct ListItem {
    pub task: bool,
    pub checked: bool,
    pub tokens: Vec<Block>,
    pub raw: String,
    pub source_marker: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TableBlock {
    pub header: Vec<Vec<Inline>>,
    pub rows: Vec<Vec<Vec<Inline>>>,
    pub raw: String,
}

fn count_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

/// Parse a markdown document into a token stream.
pub fn parse_markdown(source: &str) -> Vec<Block> {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut tokens: Vec<Block> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];

        // Blank line -> space token (kept when it separates content).
        if line.trim().is_empty() {
            tokens.push(Block::Space);
            i += 1;
            continue;
        }

        // Fenced code.
        if let Some(cap) = code_fence_open(line) {
            let (lang, marker_len) = cap;
            let mut code_lines: Vec<&str> = Vec::new();
            i += 1;
            let mut closed = false;
            while i < lines.len() {
                let l = lines[i];
                if l.trim().starts_with(&marker_len)
                    && l.trim()
                        .chars()
                        .all(|c| c == marker_len.chars().next().unwrap())
                {
                    closed = true;
                    i += 1;
                    break;
                }
                code_lines.push(l);
                i += 1;
            }
            let _ = closed;
            let text = code_lines.join("\n");
            tokens.push(Block::Code {
                lang: lang.trim().to_string(),
                text,
                raw: format!("```{lang}\n{}\n```", code_lines.join("\n")),
            });
            continue;
        }

        // ATX heading.
        if let Some(h) = atx_heading(line) {
            tokens.push(Block::Heading {
                level: h.0,
                tokens: parse_inlines(&h.1),
            });
            i += 1;
            continue;
        }

        // Horizontal rule.
        if is_hr(line) {
            tokens.push(Block::Hr);
            i += 1;
            continue;
        }

        // Blockquote.
        if line.starts_with('>') || line.trim_start().starts_with('>') {
            let mut quote_lines: Vec<String> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                let t = l.trim_start();
                if t.starts_with('>') {
                    quote_lines.push(t.trim_start_matches('>').trim_start().to_string());
                    i += 1;
                } else if l.trim().is_empty() {
                    quote_lines.push(String::new());
                    i += 1;
                } else {
                    break;
                }
            }
            // Drop trailing blanks.
            while quote_lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                quote_lines.pop();
            }
            let nested = parse_markdown(&quote_lines.join("\n"));
            tokens.push(Block::Blockquote(nested));
            continue;
        }

        // Table.
        if let Some((table, line_count)) = try_parse_table(&lines[i..]) {
            tokens.push(Block::Table(table));
            i += line_count;
            continue;
        }

        // List.
        if let Some(parsed_list) = try_parse_list(&lines[i..]) {
            tokens.push(Block::List(parsed_list.block));
            i += parsed_list.line_count;
            continue;
        }

        // Paragraph: gather until blank line, a block marker, or fence.
        let mut para_lines: Vec<&str> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty()
                || code_fence_open(l).is_some()
                || atx_heading(l).is_some()
                || is_hr(l)
                || l.trim_start().starts_with('>')
                || list_marker(l).is_some()
                || looks_like_table(&lines[i..])
            {
                break;
            }
            para_lines.push(l);
            i += 1;
        }
        let text = para_lines.join("\n");
        tokens.push(Block::Paragraph(parse_inlines(&text)));
    }

    // Trim leading/trailing space tokens.
    while tokens
        .first()
        .map(|t| matches!(t, Block::Space))
        .unwrap_or(false)
    {
        tokens.remove(0);
    }
    while tokens
        .last()
        .map(|t| matches!(t, Block::Space))
        .unwrap_or(false)
    {
        tokens.pop();
    }
    tokens
}

// ---------------------------------------------------------------------------
// Parser helpers
// ---------------------------------------------------------------------------

fn code_fence_open(line: &str) -> Option<(String, String)> {
    let t = line;
    let indent = count_indent(t);
    if indent > 3 {
        return None;
    }
    let trimmed = &t[indent..];
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run: String = trimmed.chars().take_while(|c| *c == marker).collect();
    if run.len() < 3 {
        return None;
    }
    let lang = trimmed[run.len()..].trim().to_string();
    Some((lang, run))
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    let t = line;
    let indent = count_indent(t);
    if indent > 3 {
        return None;
    }
    let trimmed = &t[indent..];
    if !trimmed.starts_with('#') {
        return None;
    }
    let run: String = trimmed.chars().take_while(|c| *c == '#').collect();
    let level = run.len();
    if level > 6 {
        return None;
    }
    let rest = trimmed[level..].trim();
    // requiring whitespace after # for valid heading
    if rest.is_empty() {
        // `#` alone is still a heading of empty text in marked.
        return Some((level, String::new()));
    }
    // Strip trailing #s
    let mut rest = rest.to_string();
    while rest.ends_with('#') && rest.trim_end_matches('#').ends_with(' ') {
        let trimmed_end = rest.trim_end_matches('#');
        rest = trimmed_end.trim_end().to_string();
    }
    Some((level, rest))
}

fn is_hr(line: &str) -> bool {
    let t = line.trim();
    let chars = t.chars().next();
    if !matches!(chars, Some('-') | Some('*') | Some('_')) {
        return false;
    }
    let mark = chars.unwrap();
    let run: Vec<char> = t.chars().filter(|c| *c == mark || *c == ' ').collect();
    let count = t.chars().filter(|c| *c == mark).count();
    run.len() == t.chars().count() && count >= 3
}

fn looks_like_table(lines: &[&str]) -> bool {
    if lines.len() < 2 {
        return false;
    }
    if !lines[0].trim().starts_with('|') {
        return false;
    }
    let sep = lines[1].trim();
    if !sep.starts_with('|') {
        return false;
    }
    let cells: Vec<&str> = sep.trim_matches('|').split('|').map(|s| s.trim()).collect();
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim_matches(':');
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

#[derive(Debug, Clone)]
struct ParsedList {
    block: ListBlock,
    line_count: usize,
}

fn list_marker(line: &str) -> Option<(String, bool, Option<usize>, usize)> {
    // Returns (marker_text, ordered, start, consumed_chars)
    let t = line;
    let indent = count_indent(t);
    // Top-level lists start at <=3 spaces; nested lists inside item
    // fragments may be deeper (marked uses 4-space nesting).
    if indent > 24 {
        return None;
    }
    let rest = &t[indent..];
    // Unordered: [-+*] followed by space or EOL
    let chars: Vec<char> = rest.chars().collect();
    if chars.is_empty() {
        return None;
    }
    if matches!(chars[0], '-' | '+' | '*') {
        if chars.len() == 1 {
            return Some((rest.to_string(), false, None, indent + 1));
        }
        if chars[1] == ' ' || chars[1] == '\t' {
            return Some((rest[..2].to_string(), false, None, indent + 2));
        }
        return None;
    }
    // Ordered: digits followed by '.' or ')'
    let mut j = 0;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j == 0 || j > 9 {
        return None;
    }
    if j < chars.len() && (chars[j] == '.' || chars[j] == ')') {
        let start: usize = rest[..j].parse().unwrap_or(1);
        if j + 1 < chars.len() && (chars[j + 1] == ' ' || chars[j + 1] == '\t') {
            return Some((
                rest[..=j + 1].to_string(),
                true,
                Some(start),
                indent + j + 2,
            ));
        }
        if j + 1 == chars.len() {
            return Some((rest[..=j].to_string(), true, Some(start), indent + j + 1));
        }
        return None;
    }
    None
}

fn try_parse_list(lines: &[&str]) -> Option<ParsedList> {
    let first = lines.first()?;
    let marker = list_marker(first)?;
    let ordered = marker.1;
    let start = marker.2.unwrap_or(1);
    let base_indent = count_indent(first);
    let mut items: Vec<ListItem> = Vec::new();
    let mut consumed = 0usize;
    let loose = false;

    while consumed < lines.len() {
        let line = lines[consumed];
        if line.trim().is_empty() {
            // Blank line ends the list; the outer parser re-triggers on the
            // next marker (upstream splits numbered lists at blank lines
            // unless an item contains internal blank content).
            let mut j = consumed;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            let _ = j;
            break;
        }

        let Some(marker_info) = list_marker(line) else {
            break;
        };
        if marker_info.1 != ordered {
            break;
        }
        let marker_text = marker_info.0;
        let marker_len = marker_info.3;

        let content = line[marker_len.min(line.len())..].to_string();
        let mut item_lines: Vec<String> = Vec::new();
        item_lines.push(content.clone());
        consumed += 1;

        // Consume continuation lines belonging to this item: wrapped text at
        // a deeper indent, or nested list items at a deeper indent.
        while consumed < lines.len() {
            let l = lines[consumed];
            if l.trim().is_empty() {
                let mut j = consumed;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                if j < lines.len() {
                    let next = lines[j];
                    let next_indent = count_indent(next);
                    let is_next_item = list_marker(next).map(|m| m.1 == ordered).unwrap_or(false);
                    if next_indent > base_indent || (is_next_item && next_indent > base_indent) {
                        item_lines.push(String::new());
                        consumed = j;
                        continue;
                    }
                    // Blank then a sibling item or list end: leave the blank
                    // for the outer parser (which emits a Space token).
                    let _ = j;
                }
                break;
            }
            let indent = count_indent(l);
            let next_is_item = list_marker(l).is_some();
            if next_is_item && indent <= base_indent {
                // Sibling item at the same (or lesser) indent.
                break;
            }
            if indent > base_indent {
                // Continuation: nested list items are kept verbatim so the
                // nested parser sees their markers; plain text is dedented.
                let is_nested_list_item = next_is_item;
                let stripped = if indent >= 4 && !is_nested_list_item {
                    l[4.min(l.len())..].to_string()
                } else if indent >= 2 && !is_nested_list_item {
                    l[indent.min(l.len())..].to_string()
                } else {
                    l.to_string()
                };
                item_lines.push(stripped);
                consumed += 1;
                continue;
            }
            break;
        }

        let raw = item_lines.join("\n");
        let mut item_tokens = parse_markdown(&raw);
        while item_tokens
            .first()
            .map(|t| matches!(t, Block::Space))
            .unwrap_or(false)
        {
            item_tokens.remove(0);
        }
        let mut task = false;
        let mut checked = false;
        let content_str = item_lines.first().cloned().unwrap_or_default();
        if let Some(rest) = content_str.strip_prefix("[ ] ") {
            task = true;
            if let Some(first_tok) = item_tokens.first_mut() {
                *first_tok = Block::Paragraph(parse_inlines(rest));
            }
        } else if let Some(rest) = content_str.strip_prefix("[x] ") {
            task = true;
            checked = true;
            if let Some(first_tok) = item_tokens.first_mut() {
                *first_tok = Block::Paragraph(parse_inlines(rest));
            }
        }

        items.push(ListItem {
            task,
            checked,
            tokens: item_tokens,
            raw,
            source_marker: Some(marker_text.clone()),
        });
    }

    Some(ParsedList {
        block: ListBlock {
            ordered,
            start,
            loose,
            items,
        },
        line_count: consumed,
    })
}

fn parse_table_separator(line: &str) -> Option<Vec<Option<&str>>> {
    let t = line.trim();
    if !t.starts_with('|') {
        return None;
    }
    let inner = t.trim_start_matches('|').trim_end_matches('|');
    let cells: Vec<&str> = inner.split('|').map(|s| s.trim()).collect();
    let mut aligns = Vec::new();
    for c in cells {
        let c = c.trim();
        if c.is_empty() || !c.chars().all(|ch| ch == '-' || ch == ':') {
            return None;
        }
        aligns.push(None);
    }
    Some(aligns)
}

fn try_parse_table(lines: &[&str]) -> Option<(TableBlock, usize)> {
    if lines.len() < 2 {
        return None;
    }
    // Header row must contain a '|'.
    let header_raw = lines[0];
    if !header_raw.trim().starts_with('|') {
        return None;
    }
    let sep = parse_table_separator(lines[1])?;
    let ncols = sep.len();
    let header: Vec<Vec<Inline>> = header_raw
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| parse_inlines(cell.trim()))
        .collect();
    if header.len() != ncols {
        return None;
    }
    let mut rows = Vec::new();
    let mut consumed = 2usize;
    while consumed < lines.len() {
        let l = lines[consumed];
        if l.trim().is_empty() || !l.trim().starts_with('|') {
            break;
        }
        let cells: Vec<Vec<Inline>> = l
            .trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(|cell| parse_inlines(cell.trim()))
            .collect();
        rows.push(cells);
        consumed += 1;
    }
    let raw = lines[..consumed].join("\n");
    Some((TableBlock { header, rows, raw }, consumed))
}

// ---------------------------------------------------------------------------
// Inline parsing
// ---------------------------------------------------------------------------

fn parse_inlines(source: &str) -> Vec<Inline> {
    parse_inline_range(source, 0, source.len())
}

fn parse_inline_range(source: &str, start: usize, end: usize) -> Vec<Inline> {
    let mut tokens: Vec<Inline> = Vec::new();
    let mut text_buf = String::new();

    let flush = |tokens: &mut Vec<Inline>, text_buf: &mut String| {
        if !text_buf.is_empty() {
            tokens.push(Inline::Text(std::mem::take(text_buf)));
        }
    };

    let mut i = start;
    while i < end {
        let rest = &source[i..end];
        let c = rest.chars().next().unwrap();

        // Escapes.
        if c == '\\' && rest.len() > 1 {
            let next = rest.chars().nth(1).unwrap();
            if "`*_{}[]()#+-.!|>~\\".contains(next) {
                // Preserve the backslash when configured.
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Escape(next.to_string()));
                i += 1 + next.len_utf8();
                continue;
            }
        }

        // Strikethrough ~~ (strict: not followed by ~ or whitespace-start).
        if rest.starts_with("~~") && !rest[2..].starts_with(' ') && !rest[2..].starts_with('~') {
            if let Some(close_rel) = find_del_end(&rest[2..]) {
                let close = 2 + close_rel;
                let inner = &rest[2..close];
                if !inner.is_empty() {
                    flush(&mut tokens, &mut text_buf);
                    tokens.push(Inline::Del(parse_inline_range(source, i + 2, i + close)));
                    i += close;
                    continue;
                }
            }
        }

        // Codespan.
        if c == '`' {
            let run: String = rest.chars().take_while(|ch| *ch == '`').collect();
            let fence = run.len();
            let search = &rest[fence..];
            if let Some(rel) = search.find(&"`".repeat(fence)) {
                let inner = &search[..rel];
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Codespan(inner.replace('\n', " ")));
                i += fence + rel + fence;
                continue;
            }
        }

        // Bold with **.
        if rest.starts_with("**") && !rest[2..].starts_with(' ') && !rest[2..].starts_with('*') {
            if let Some(close) = find_matching_inline_marker(&rest[2..], "**") {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Strong(parse_inline_range(
                    source,
                    i + 2,
                    i + 2 + close,
                )));
                i += 2 + close + 2;
                continue;
            }
        }

        // Italic with * (single, not **).
        if c == '*' && !rest.starts_with("**") {
            if let Some(close) = find_matching_inline_marker(&rest[1..], "*") {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Em(parse_inline_range(source, i + 1, i + 1 + close)));
                i += 1 + close + 1;
                continue;
            }
        }

        // Bold with __.
        if let Some(rest_after) = rest.strip_prefix("__") {
            if let Some(close) = find_matching_inline_marker(rest_after, "__") {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Strong(parse_inline_range(
                    source,
                    i + 2,
                    i + 2 + close,
                )));
                i += 2 + close + 2;
                continue;
            }
        }

        // Italic with _ (single).
        if c == '_' && !rest.starts_with("__") {
            if let Some(close) = find_matching_inline_marker(&rest[1..], "_") {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Em(parse_inline_range(source, i + 1, i + 1 + close)));
                i += 1 + close + 1;
                continue;
            }
        }

        // Inline LaTeX.
        if c == '$' {
            if let Some((text, raw, pending, consumed)) = try_inline_latex(rest) {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Latex { text, raw, pending });
                i += consumed;
                continue;
            }
        }
        if rest.starts_with("\\(") || rest.starts_with("\\[") {
            if let Some((text, raw, pending, consumed)) = try_inline_latex_brace(rest) {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Latex { text, raw, pending });
                i += consumed;
                continue;
            }
        }

        // Link [text](href) — only when it closes on this line.
        if c == '[' {
            if let Some((text_raw, href, consumed)) = try_inline_link(rest) {
                flush(&mut tokens, &mut text_buf);
                tokens.push(Inline::Link {
                    text: parse_inline_range(&text_raw, 0, text_raw.len()),
                    href,
                    raw_text: text_raw.clone(),
                });
                i += consumed;
                continue;
            }
        }

        // Hard break: backslash-newline or two trailing spaces (block level).
        if c == '\n' {
            // A single newline inside a paragraph is a soft break -> space.
            flush(&mut tokens, &mut text_buf);
            tokens.push(Inline::Text(" ".to_string()));
            i += 1;
            continue;
        }

        text_buf.push(c);
        i += c.len_utf8();
    }
    flush(&mut tokens, &mut text_buf);
    tokens
}

fn find_matching_inline_marker(s: &str, marker: &str) -> Option<usize> {
    let mut idx = 0usize;
    while idx + marker.len() <= s.len() {
        if &s[idx..idx + marker.len()] == marker {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

fn find_del_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'~' && bytes[i + 1] == b'~' {
            let after = i + 2;
            if after == s.len() {
                return Some(i);
            }
            let next = s[after..].chars().next().unwrap();
            if next != '~' {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn try_inline_latex(s: &str) -> Option<(String, String, bool, usize)> {
    if s.starts_with("$$") || (s.starts_with('$') && s.len() > 1 && s[1..].starts_with(' ')) {
        return None;
    }
    let opening = 1usize;
    let mut idx = opening;
    let mut escaped = false;
    while idx < s.len() {
        let c = s[idx..].chars().next().unwrap();
        if c == '\\' {
            escaped = !escaped;
            idx += 1;
            continue;
        }
        if c == '$' && !escaped {
            let body = &s[opening..idx];
            let raw = &s[..idx + 1];
            if !body.is_empty() && !body.contains('\n') {
                // Heuristic guards from upstream: currency/identifiers.
                if body.ends_with(' ') || body.starts_with(' ') {
                    return None;
                }
                if idx + 1 < s.len()
                    && s[idx + 1..]
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                {
                    return None;
                }
                if is_identifier_like(body) && idx + 1 < s.len() {
                    let nxt = s[idx + 1..].chars().next().unwrap();
                    if nxt.is_ascii_alphanumeric() || nxt == '_' {
                        return None;
                    }
                }
                return Some((body.to_string(), raw.to_string(), false, idx + 1));
            }
            return None;
        }
        escaped = false;
        idx += c.len_utf8();
    }
    // Pending (unclosed) math.
    let body = &s[opening..];
    if looks_like_pending_math(body) {
        return Some((body.to_string(), s.to_string(), true, s.len()));
    }
    None
}

fn try_inline_latex_brace(s: &str) -> Option<(String, String, bool, usize)> {
    let close_marker = if s.starts_with("\\(") { "\\)" } else { "\\]" };
    if let Some(rel) = s[2..].find(close_marker) {
        let body = &s[2..2 + rel];
        if body.is_empty() || body.contains('\n') {
            return None;
        }
        let raw = &s[..2 + rel + 2];
        return Some((body.to_string(), raw.to_string(), false, 2 + rel + 2));
    }
    Some((s[2..].to_string(), s.to_string(), true, s.len()))
}

fn is_identifier_like(s: &str) -> bool {
    // /^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$/
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return false;
    }
    if !(chars[0].is_ascii_uppercase() || chars[0] == '_') {
        return false;
    }
    let mut i = 0;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    if i == chars.len() {
        return true;
    }
    if i == chars.len() - 1 && !chars[i].is_ascii_alphanumeric() && !chars[i].is_whitespace() {
        return true;
    }
    false
}

fn looks_like_pending_math(s: &str) -> bool {
    if s.starts_with('\\') {
        return true;
    }
    s.chars().any(|c| {
        matches!(
            c,
            '\\' | '_'
                | '^'
                | '='
                | '+'
                | '*'
                | '/'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '|'
                | '±'
                | '≤'
                | '≥'
                | '≠'
                | '≈'
                | '∈'
                | '→'
                | '⇒'
                | '∞'
                | '∫'
                | '∑'
                | '√'
                | '-'
        )
    })
}

fn try_inline_link(s: &str) -> Option<(String, String, usize)> {
    // find unescaped ']', then '(' ... ')'
    let mut i = 1usize;
    let mut escaped = false;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if c == '\\' {
            escaped = !escaped;
            i += 1;
            continue;
        }
        if c == ']' && !escaped {
            let text = &s[1..i];
            let rest = &s[i + 1..];
            if let Some(href_start_rel) = rest.find("(") {
                if href_start_rel == 0 {
                    if let Some(close_rel) = rest.find(")") {
                        let href = rest[1..close_rel].to_string();
                        return Some((text.to_string(), href, i + 1 + close_rel + 1));
                    }
                }
            }
            return None;
        }
        escaped = false;
        i += c.len_utf8();
    }
    None
}

// ---------------------------------------------------------------------------
// Markdown component
// ---------------------------------------------------------------------------

/// The Markdown component.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    default_text_style: Option<DefaultTextStyle>,
    theme: MarkdownTheme,
    options: MarkdownOptions,

    // Cache (interior-mutable for render under &self).
    cached_text: std::sync::Mutex<Option<String>>,
    cached_width: std::sync::Mutex<Option<usize>>,
    cached_lines: std::sync::Mutex<Option<Vec<String>>>,
}

impl std::fmt::Debug for Markdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Markdown")
            .field("text.len", &self.text.len())
            .finish()
    }
}

impl Markdown {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: Option<MarkdownOptions>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            default_text_style,
            theme,
            options: options.unwrap_or_default(),
            cached_text: std::sync::Mutex::new(None),
            cached_width: std::sync::Mutex::new(None),
            cached_lines: std::sync::Mutex::new(None),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn render_document(&self, width: usize) -> Vec<String> {
        let content_width = std::cmp::max(1, width.saturating_sub(self.padding_x * 2));
        let text = match &self.options.transform {
            Some(transform) => transform(&self.text, content_width),
            None => self.text.clone(),
        };

        if text.trim().is_empty() {
            return Vec::new();
        }

        let normalized_text = text.replace('\t', "   ");
        let tokens = parse_markdown(&normalized_text);

        let mut rendered_lines: Vec<String> = Vec::new();
        for i in 0..tokens.len() {
            let next_type = tokens.get(i + 1).map(block_type_name);
            let token_lines = self.render_token(&tokens[i], content_width, next_type, None);
            for tl in token_lines {
                rendered_lines.push(tl);
            }
        }

        // Wrap lines (no padding).
        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in rendered_lines {
            if is_image_line(&line) {
                wrapped_lines.push(line);
            } else {
                for wl in wrap_text_with_ansi(&line, content_width) {
                    wrapped_lines.push(wl);
                }
            }
        }

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let mut content_lines: Vec<String> = Vec::new();
        let bg_fn = self
            .default_text_style
            .as_ref()
            .and_then(|s| s.bg_color.as_ref());
        for line in wrapped_lines {
            if is_image_line(&line) {
                content_lines.push(line);
                continue;
            }
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            match bg_fn {
                Some(bg) => {
                    content_lines.push(apply_background_to_line(&line_with_margins, width, &**bg))
                }
                None => {
                    let visible_len = visible_width(&line_with_margins);
                    content_lines.push(format!(
                        "{line_with_margins}{}",
                        " ".repeat(width.saturating_sub(visible_len))
                    ));
                }
            }
        }

        let empty_line = " ".repeat(width);
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            match bg_fn {
                Some(bg) => result.push(apply_background_to_line(&empty_line, width, &**bg)),
                None => result.push(empty_line.clone()),
            }
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            match bg_fn {
                Some(bg) => result.push(apply_background_to_line(&empty_line, width, &**bg)),
                None => result.push(empty_line.clone()),
            }
        }

        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }

    fn apply_default_style(&self, text: &str) -> String {
        let Some(style) = &self.default_text_style else {
            return text.to_string();
        };
        let mut styled = text.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    fn get_default_style_prefix(&self) -> String {
        let Some(style) = &self.default_text_style else {
            return String::new();
        };
        let sentinel = "\u{0}";
        let mut styled = sentinel.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        match styled.find(sentinel) {
            Some(i) => styled[..i].to_string(),
            None => String::new(),
        }
    }

    fn get_style_prefix(&self, style_fn: &dyn Fn(&str) -> String) -> String {
        let sentinel = "\u{0}";
        let styled = style_fn(sentinel);
        match styled.find(sentinel) {
            Some(i) => styled[..i].to_string(),
            None => String::new(),
        }
    }

    /// The ANSI prefix that restores the default style after inline tokens.
    fn style_prefix(&self) -> String {
        self.get_default_style_prefix()
    }
}

/// A style context carries the ANSI prefix used to restore surrounding style
/// after inline element styling.
#[derive(Clone, Default)]
struct InlineStyleContext {
    style_prefix: String,
}

impl Markdown {
    /// Apply a style function to text, returning styled + prefix.
    fn render_inline_tokens(&self, tokens: &[Inline], context: &InlineStyleContext) -> String {
        let mut result = String::new();
        let style_prefix = context.style_prefix.clone();

        for token in tokens {
            match token {
                Inline::Latex { text, raw, pending } => {
                    let rendered = if !pending && self.options.render_latex {
                        render_latex(text, false).unwrap_or_else(|| raw.clone())
                    } else {
                        raw.clone()
                    };
                    result.push_str(&self.apply_text_with_newlines(&rendered, context));
                }
                Inline::Escape(text) => {
                    if self.options.preserve_backslash_escapes {
                        result.push_str(
                            &self.apply_text_with_newlines(&format!("\\{text}"), context),
                        );
                    } else {
                        result.push_str(&self.apply_text_with_newlines(text, context));
                    }
                }
                Inline::Text(text) => {
                    result.push_str(&self.apply_text_with_newlines(text, context));
                }
                Inline::Strong(inner) => {
                    let content = self.render_inline_tokens(inner, context);
                    result.push_str(&(self.theme.bold)(&content));
                    result.push_str(&style_prefix);
                }
                Inline::Em(inner) => {
                    let content = self.render_inline_tokens(inner, context);
                    result.push_str(&(self.theme.italic)(&content));
                    result.push_str(&style_prefix);
                }
                Inline::Del(inner) => {
                    let content = self.render_inline_tokens(inner, context);
                    result.push_str(&(self.theme.strikethrough)(&content));
                    result.push_str(&style_prefix);
                }
                Inline::Codespan(text) => {
                    result.push_str(&(self.theme.code)(text));
                    result.push_str(&style_prefix);
                }
                Inline::Link {
                    text: inner,
                    href,
                    raw_text,
                } => {
                    let link_text = self.render_inline_tokens(inner, context);
                    let styled = (self.theme.link)(&(self.theme.underline)(&link_text));
                    if get_capabilities().hyperlinks {
                        result.push_str(&hyperlink(&styled, href));
                        result.push_str(&style_prefix);
                    } else {
                        let href_for_comparison = href.strip_prefix("mailto:").unwrap_or(href);
                        if raw_text == href || raw_text == href_for_comparison {
                            result.push_str(&styled);
                            result.push_str(&style_prefix);
                        } else {
                            result.push_str(&styled);
                            result.push_str(&(self.theme.link_url)(&format!(" ({href})")));
                            result.push_str(&style_prefix);
                        }
                    }
                }
                Inline::Br => {
                    result.push('\n');
                }
                Inline::Html(raw) => {
                    result.push_str(&self.apply_text_with_newlines(raw, context));
                }
            }
        }

        while !style_prefix.is_empty() && result.ends_with(&style_prefix) {
            result.truncate(result.len() - style_prefix.len());
        }
        result
    }

    fn apply_text_with_newlines(&self, text: &str, _context: &InlineStyleContext) -> String {
        let segments: Vec<&str> = text.split('\n').collect();
        let mut out = Vec::new();
        for segment in segments {
            out.push(self.apply_default_style(segment));
        }
        out.join("\n")
    }

    fn render_token(
        &self,
        token: &Block,
        width: usize,
        _next_token_type: Option<&str>,
        _context: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let context = _context.cloned().unwrap_or(InlineStyleContext {
            style_prefix: self.style_prefix(),
        });
        let mut lines: Vec<String> = Vec::new();

        match token {
            Block::Heading { level, tokens } => {
                let heading_prefix = format!("{} ", "#".repeat(*level));
                let style_fn: Box<dyn Fn(&str) -> String> = if *level == 1 {
                    Box::new(|t: &str| {
                        (self.theme.heading)(&(self.theme.bold)(&(self.theme.underline)(t)))
                    })
                } else {
                    Box::new(|t: &str| (self.theme.heading)(&(self.theme.bold)(t)))
                };
                let prefix = self.get_style_prefix(&style_fn);
                let heading_ctx = InlineStyleContext {
                    style_prefix: prefix.clone(),
                };
                let heading_text = self.render_inline_tokens(tokens, &heading_ctx);
                let styled_heading = if *level >= 3 {
                    style_fn(&heading_prefix) + &heading_text
                } else {
                    heading_text
                };
                lines.push(styled_heading);
            }
            Block::Paragraph(tokens) => {
                let para_text = self.render_inline_tokens(tokens, &context);
                lines.push(para_text);
            }
            Block::Text(tokens) => {
                lines.push(self.render_inline_tokens(tokens, &context));
            }
            Block::LatexBlock { text, raw, pending } => {
                let rendered = if !pending && self.options.render_latex {
                    render_latex(text, true).unwrap_or_else(|| raw.trim().to_string())
                } else {
                    raw.trim().to_string()
                };
                for line in rendered.split('\n') {
                    lines.push(self.apply_default_style(line));
                }
            }
            Block::Code { lang, text, raw } => {
                let indent = self
                    .theme
                    .code_block_indent
                    .clone()
                    .unwrap_or_else(|| "  ".to_string());
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                match &self.theme.highlight_code {
                    Some(highlight) => {
                        for hl in highlight(text, Some(lang)) {
                            lines.push(format!("{indent}{hl}"));
                        }
                    }
                    None => {
                        for code_line in text.split('\n') {
                            lines.push(format!("{indent}{}", (self.theme.code_block)(code_line)));
                        }
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                let _ = raw;
            }
            Block::List(list) => {
                lines.extend(self.render_list(list, 0, width, &context));
            }
            Block::Table(table) => {
                lines.extend(self.render_table(table, width, &context));
            }
            Block::Blockquote(inner_tokens) => {
                let quote_style: Box<dyn Fn(&str) -> String> =
                    Box::new(|t: &str| (self.theme.quote)(&(self.theme.italic)(t)));
                let quote_style_prefix = self.get_style_prefix(&quote_style);
                let quote_content_width = std::cmp::max(1, width.saturating_sub(2));

                let mut rendered_quote_lines: Vec<String> = Vec::new();
                for (i, qt) in inner_tokens.iter().enumerate() {
                    let next = inner_tokens.get(i + 1).map(block_type_name);
                    rendered_quote_lines.extend(self.render_token(
                        qt,
                        quote_content_width,
                        next,
                        Some(&InlineStyleContext {
                            style_prefix: quote_style_prefix.clone(),
                        }),
                    ));
                }
                while rendered_quote_lines
                    .last()
                    .map(|l| l.is_empty())
                    .unwrap_or(false)
                {
                    rendered_quote_lines.pop();
                }

                for quote_line in rendered_quote_lines {
                    let styled_line = if quote_style_prefix.is_empty() {
                        quote_style(&quote_line)
                    } else {
                        let reapply =
                            quote_line.replace("\x1b[0m", &format!("\x1b[0m{quote_style_prefix}"));
                        quote_style(&reapply)
                    };
                    for wrapped in wrap_text_with_ansi(&styled_line, quote_content_width) {
                        lines.push(format!("{}{wrapped}", (self.theme.quote_border)("│ ")));
                    }
                }
            }
            Block::Hr => {
                lines.push((self.theme.hr)(&"─".repeat(std::cmp::min(width, 80))));
            }
            Block::Html(raw) => {
                lines.push(self.apply_default_style(raw.trim()));
            }
            Block::Space => {
                lines.push(String::new());
            }
        }

        lines
    }

    fn render_list(
        &self,
        token: &ListBlock,
        depth: usize,
        width: usize,
        context: &InlineStyleContext,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let indent = "    ".repeat(depth);
        let start_number = token.start.max(1);

        for (i, item) in token.items.iter().enumerate() {
            let is_last_item = i == token.items.len() - 1;
            let bullet = if token.ordered {
                if self.options.preserve_ordered_list_markers {
                    item.source_marker
                        .clone()
                        .unwrap_or_else(|| format!("{}. ", start_number + i))
                } else {
                    format!("{}. ", start_number + i)
                }
            } else if self.options.preserve_ordered_list_markers {
                item.source_marker
                    .clone()
                    .unwrap_or_else(|| "- ".to_string())
            } else {
                "- ".to_string()
            };

            let task_marker = if item.task {
                if item.checked {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let marker = format!("{bullet}{task_marker}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = std::cmp::max(1, width.saturating_sub(visible_width(&first_prefix)));
            let mut rendered_any_line = false;

            for item_token in &item.tokens {
                if matches!(item_token, Block::List(_)) {
                    if let Block::List(nested) = item_token {
                        lines.extend(self.render_list(nested, depth + 1, width, context));
                        rendered_any_line = true;
                    }
                    continue;
                }

                let item_lines = if item_width == 0 {
                    Vec::new()
                } else {
                    let next = None;
                    self.render_token(item_token, item_width, next, Some(context))
                };
                for line in item_lines {
                    for wrapped in wrap_text_with_ansi(&line, item_width) {
                        let line_prefix = if rendered_any_line {
                            continuation_prefix.clone()
                        } else {
                            first_prefix.clone()
                        };
                        lines.push(format!("{line_prefix}{wrapped}"));
                        rendered_any_line = true;
                    }
                }
            }

            if !rendered_any_line {
                lines.push(first_prefix);
            }

            if token.loose && !is_last_item {
                lines.push(String::new());
            }
        }

        lines
    }

    fn get_longest_word_width(&self, text: &str, max_width: Option<usize>) -> usize {
        let longest = text
            .split(char::is_whitespace)
            .filter(|w| !w.is_empty())
            .map(visible_width)
            .max()
            .unwrap_or(0);
        match max_width {
            Some(m) => longest.min(m),
            None => longest,
        }
    }

    fn wrap_cell_text(&self, text: &str, max_width: usize, style_prefix: &str) -> Vec<String> {
        let lines = wrap_text_with_ansi(text, std::cmp::max(1, max_width));
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let style_reset = if index < lines.len() - 1 {
                    "\x1b[22;23;24;25;27;28;29;39m"
                } else {
                    ""
                };
                format!("{line}{style_reset}{style_prefix}")
            })
            .collect()
    }

    fn render_table(
        &self,
        token: &TableBlock,
        available_width: usize,
        context: &InlineStyleContext,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let num_cols = token.header.len();
        if num_cols == 0 {
            return lines;
        }

        let border_overhead = 3 * num_cols + 1;
        let available_for_cells = available_width.saturating_sub(border_overhead);
        if available_for_cells < num_cols {
            let fallback = wrap_text_with_ansi(&token.raw, available_width);
            lines.extend(fallback);
            return lines;
        }

        const MAX_UNBROKEN_WORD_WIDTH: usize = 30;
        let mut natural_widths: Vec<usize> = Vec::new();
        let mut min_word_widths: Vec<usize> = Vec::new();
        for h in &token.header {
            let header_text = self.render_inline_tokens(h, context);
            natural_widths.push(visible_width(&header_text));
            min_word_widths.push(std::cmp::max(
                1,
                self.get_longest_word_width(&header_text, Some(MAX_UNBROKEN_WORD_WIDTH)),
            ));
        }
        for row in &token.rows {
            for i in 0..row.len() {
                let cell_text = self.render_inline_tokens(&row[i], context);
                natural_widths[i] = std::cmp::max(natural_widths[i], visible_width(&cell_text));
                min_word_widths[i] = std::cmp::max(
                    min_word_widths[i],
                    self.get_longest_word_width(&cell_text, Some(MAX_UNBROKEN_WORD_WIDTH)),
                );
            }
        }

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: usize = min_column_widths.iter().sum();
        if min_cells_width > available_for_cells {
            min_column_widths = vec![1; num_cols];
            let remaining = available_for_cells.saturating_sub(num_cols);
            if remaining > 0 {
                let total_weight: usize = min_word_widths.iter().map(|w| w.saturating_sub(1)).sum();
                let mut growth: Vec<usize> = min_word_widths
                    .iter()
                    .map(|w| checked_div(w.saturating_sub(1), remaining, total_weight))
                    .collect();
                let allocated: usize = growth.iter().sum();
                let mut leftover = remaining.saturating_sub(allocated);
                for i in 0..num_cols {
                    if leftover == 0 {
                        break;
                    }
                    min_column_widths[i] += growth[i];
                    growth[i] = 0;
                    if leftover > 0 {
                        min_column_widths[i] += 1;
                        leftover -= 1;
                    }
                }
            }
            min_cells_width = min_column_widths.iter().sum();
        }

        let total_natural_width: usize = natural_widths.iter().sum::<usize>() + border_overhead;
        let column_widths: Vec<usize> = if total_natural_width <= available_width {
            natural_widths
                .iter()
                .enumerate()
                .map(|(index, w)| (*w).max(min_column_widths[index]))
                .collect()
        } else {
            let total_grow_potential: usize = natural_widths
                .iter()
                .enumerate()
                .map(|(index, w)| w.saturating_sub(min_column_widths[index]))
                .sum();
            let extra_width = available_for_cells.saturating_sub(min_cells_width);
            let mut widths: Vec<usize> = min_column_widths
                .iter()
                .enumerate()
                .map(|(index, min_w)| {
                    let natural_w = natural_widths[index];
                    let min_delta = natural_w.saturating_sub(*min_w);
                    let grow = checked_div(min_delta, extra_width, total_grow_potential);
                    min_w + grow
                })
                .collect();
            // Distribute rounding leftovers.
            let allocated: usize = widths.iter().sum();
            let mut remaining = available_for_cells.saturating_sub(allocated);
            while remaining > 0 {
                let mut grew = false;
                for i in 0..num_cols {
                    if remaining == 0 {
                        break;
                    }
                    if widths[i] < natural_widths[i] {
                        widths[i] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            widths
        };

        // Top border.
        lines.push(format!(
            "┌─{}─┐",
            column_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┬─")
        ));

        // Header.
        let header_cell_lines: Vec<Vec<String>> = token
            .header
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let text = self.render_inline_tokens(cell, context);
                self.wrap_cell_text(&text, column_widths[i], &context.style_prefix)
            })
            .collect();
        let header_line_count = header_cell_lines.iter().map(|c| c.len()).max().unwrap_or(0);
        for line_idx in 0..header_line_count {
            let row_parts: Vec<String> = header_cell_lines
                .iter()
                .enumerate()
                .map(|(col_idx, cell_lines)| {
                    let text = cell_lines.get(line_idx).cloned().unwrap_or_default();
                    let padded = format!(
                        "{text}{}",
                        " ".repeat(column_widths[col_idx].saturating_sub(visible_width(&text)))
                    );
                    (self.theme.bold)(&padded)
                })
                .collect();
            lines.push(format!("│ {} │", row_parts.join(" │ ")));
        }

        // Separator.
        lines.push(format!(
            "├─{}─┤",
            column_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─")
        ));

        // Rows.
        for (row_index, row) in token.rows.iter().enumerate() {
            let row_cell_lines: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    let text = self.render_inline_tokens(cell, context);
                    self.wrap_cell_text(&text, column_widths[i], &context.style_prefix)
                })
                .collect();
            let row_line_count = row_cell_lines.iter().map(|c| c.len()).max().unwrap_or(0);
            for line_idx in 0..row_line_count {
                let row_parts: Vec<String> = row_cell_lines
                    .iter()
                    .enumerate()
                    .map(|(col_idx, cell_lines)| {
                        let text = cell_lines.get(line_idx).cloned().unwrap_or_default();
                        format!(
                            "{text}{}",
                            " ".repeat(column_widths[col_idx].saturating_sub(visible_width(&text)))
                        )
                    })
                    .collect();
                lines.push(format!("│ {} │", row_parts.join(" │ ")));
            }
            if row_index < token.rows.len() - 1 {
                lines.push(format!(
                    "├─{}─┤",
                    column_widths
                        .iter()
                        .map(|w| "─".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("─┼─")
                ));
            }
        }

        // Bottom border.
        lines.push(format!(
            "└─{}─┘",
            column_widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┴─")
        ));

        let _ = available_for_cells;
        lines
    }
}

fn checked_div(numerator: usize, scale: usize, denom: usize) -> usize {
    if denom == 0 {
        return 0;
    }
    // Guard against overflow with u128 intermediate.
    ((numerator as u128 * scale as u128) / denom as u128) as usize
}

fn block_type_name(block: &Block) -> &'static str {
    match block {
        Block::Space => "space",
        Block::List(_) => "list",
        Block::Code { .. } => "code",
        _ => "other",
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        {
            let cached_text = self.cached_text.lock().unwrap().clone();
            let cached_width = *self.cached_width.lock().unwrap();
            let cached_lines = self.cached_lines.lock().unwrap().clone();
            if let (Some(t), Some(w), Some(lines)) = (cached_text, cached_width, cached_lines) {
                if t == self.text && w == width {
                    return lines;
                }
            }
        }
        let result = self.render_document(width);
        *self.cached_text.lock().unwrap() = Some(self.text.clone());
        *self.cached_width.lock().unwrap() = Some(width);
        *self.cached_lines.lock().unwrap() = Some(result.clone());
        result
    }

    fn invalidate(&mut self) {
        *self.cached_text.lock().unwrap() = None;
        *self.cached_width.lock().unwrap() = None;
        *self.cached_lines.lock().unwrap() = None;
    }
}

#[path = "markdown_tests.rs"]
#[cfg(test)]
mod markdown_tests;
