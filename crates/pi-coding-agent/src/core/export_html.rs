//! Session → HTML export — port of
//! `packages/coding-agent/src/core/export-html/index.ts`.
//!
//! Renders a session JSONL file into a self-contained, zero-JavaScript HTML
//! document with theme-colored CSS.
//!
//! The export is intentionally static: Rust renders the session tree, header,
//! Markdown subset, messages, and tool calls/results before writing the file.
//! It does not depend on a browser runtime, `marked`, or `highlight.js`.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use pi_tui::components::markdown::{Block, Inline, ListBlock, TableBlock};
use serde_json::{Map, Value};

use crate::config::APP_NAME;
use crate::theme;

const TEMPLATE_HTML: &str = include_str!("../../data/export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../../data/export-html/template.css");

/// Tools rendered directly by the HTML template (not pre-rendered via
/// TUI→ANSI→HTML pipeline); mirrors upstream `TEMPLATE_RENDERED_TOOLS`.
const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// Error type mirroring upstream thrown Error messages.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{0}")]
    Message(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl ExportError {
    fn msg(message: impl Into<String>) -> Self {
        ExportError::Message(message.into())
    }
}

// ---------------------------------------------------------------------------
// Color helpers (export-html/index.ts)
// ---------------------------------------------------------------------------

fn parse_color(color: &str) -> Option<(u8, u8, u8)> {
    // hex #RRGGBB
    if color.starts_with('#') && color.len() == 7 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&color[1..3], 16),
            u8::from_str_radix(&color[3..5], 16),
            u8::from_str_radix(&color[5..7], 16),
        ) {
            return Some((r, g, b));
        }
    }
    // Upstream accepts optional whitespace between `rgb` and `(` and around
    // each component (`/^rgb\s*\(\s*(\d+)\s*,.../`).
    if let Some(rest) = color
        .strip_prefix("rgb")
        .and_then(|s| s.trim_start().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = rest.split(',').map(|s| s.trim()).collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].parse::<u8>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                return Some((r, g, b));
            }
        }
    }
    None
}

fn get_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let to_linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r as f64) + 0.7152 * to_linear(g as f64) + 0.0722 * to_linear(b as f64)
}

/// Factor > 1 lightens, < 1 darkens (clamped to u8).
fn adjust_brightness(color: &str, factor: f64) -> String {
    match parse_color(color) {
        None => color.to_string(),
        Some((r, g, b)) => {
            let adj = |c: u8| ((c as f64 * factor).round()).clamp(0.0, 255.0) as u8;
            format!("rgb({}, {}, {})", adj(r), adj(g), adj(b))
        }
    }
}

/// Upstream `deriveExportColors` — page/card/info backgrounds derived from a
/// base color when a theme doesn't specify explicit export colors.
fn derive_export_colors(base_color: &str) -> (String, String, String) {
    match parse_color(base_color) {
        None => (
            "rgb(24, 24, 30)".to_string(),
            "rgb(30, 30, 36)".to_string(),
            "rgb(60, 55, 40)".to_string(),
        ),
        Some((r, g, b)) => {
            let luminance = get_luminance((r, g, b));
            if luminance > 0.5 {
                (
                    adjust_brightness(base_color, 0.96),
                    base_color.to_string(),
                    format!(
                        "rgb({}, {}, {})",
                        (r as u16 + 10).min(255),
                        (g as u16 + 5).min(255),
                        (b as u16).saturating_sub(20)
                    ),
                )
            } else {
                (
                    adjust_brightness(base_color, 0.7),
                    adjust_brightness(base_color, 0.85),
                    format!(
                        "rgb({}, {}, {})",
                        (r as u16 + 20).min(255),
                        (g as u16 + 15).min(255),
                        b
                    ),
                )
            }
        }
    }
}

/// Upstream `generateThemeVars` — CSS custom properties for the theme.
fn generate_theme_vars(theme_name: Option<&str>) -> Result<String, ExportError> {
    let colors = theme::get_resolved_theme_colors(theme_name)
        .map_err(|e| ExportError::msg(format!("Failed to resolve theme colors: {e}")))?;
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in &colors {
        lines.push(format!("--{key}: {value};"));
    }
    let theme_export = theme::get_theme_export_colors(theme_name).unwrap_or_default();
    let user_message_bg = colors
        .get("userMessageBg")
        .cloned()
        .unwrap_or_else(|| "#343541".to_string());
    let derived = derive_export_colors(&user_message_bg);
    let page_bg = theme_export.page_bg.unwrap_or_else(|| derived.0.clone());
    let card_bg = theme_export.card_bg.unwrap_or_else(|| derived.1.clone());
    let info_bg = theme_export.info_bg.unwrap_or_else(|| derived.2.clone());
    lines.push(format!("--exportPageBg: {page_bg};"));
    lines.push(format!("--exportCardBg: {card_bg};"));
    lines.push(format!("--exportInfoBg: {info_bg};"));
    Ok(lines.join("\n      "))
}

/// The rendered session payload passed to the template (mirrors upstream
/// `SessionData`; omitted keys stay absent so JSON output matches).
pub struct SessionData {
    pub header: Value,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<Value>>,
    pub rendered_tools: Option<Map<String, Value>>,
}

/// JS `String.prototype.replace(searchString, replacement)` semantics with no
/// capture groups (ES GetSubstitution): `$$` -> `$`, `$&` -> the matched
/// string, ``$` `` -> text before the match, `$'` -> text after the match,
/// `$n` -> literal `$n` (no groups), other `$` -> literal. Replaces the
/// FIRST occurrence only, matching JS.
pub fn js_replace(haystack: &str, search: &str, replacement: &str) -> String {
    let Some(rel_pos) = haystack.find(search) else {
        return haystack.to_string();
    };
    let before = &haystack[..rel_pos];
    let after = &haystack[rel_pos + search.len()..];
    let mut out = String::with_capacity(before.len() + replacement.len() + after.len());
    out.push_str(before);
    let mut chars = replacement.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
            }
            Some((_, '&')) => {
                chars.next();
                out.push_str(search);
            }
            Some((_, '`')) => {
                chars.next();
                out.push_str(before);
            }
            Some((_, '\'')) => {
                chars.next();
                out.push_str(after);
            }
            Some((_, d)) if d.is_ascii_digit() => {
                // ES parses up to two digits; with no capture groups the
                // result is the literal `$` + digits.
                let mut digits = String::new();
                while digits.len() < 2 {
                    match chars.peek() {
                        Some((_, d2)) if d2.is_ascii_digit() => {
                            digits.push(*d2);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                out.push('$');
                out.push_str(&digits);
            }
            _ => out.push('$'),
        }
    }
    out.push_str(after);
    out
}

// ---------------------------------------------------------------------------
// Static Rust renderer
// ---------------------------------------------------------------------------

/// Escape a value for use in HTML text or a quoted HTML attribute.
fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Keep the URL schemes accepted by the upstream exporter, while rejecting
/// executable schemes before a URL reaches an HTML attribute.
fn sanitize_url(value: &str) -> Option<String> {
    let url: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .collect();
    if url.is_empty() {
        return None;
    }

    let Some(colon) = url.find(':') else {
        return Some(url);
    };
    let scheme = &url[..colon];
    if scheme.is_empty()
        || !scheme.chars().enumerate().all(|(index, character)| {
            if index == 0 {
                character.is_ascii_alphabetic()
            } else {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            }
        })
    {
        return Some(url);
    }

    if ["http", "https", "mailto", "tel", "ftp"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    {
        Some(url)
    } else {
        None
    }
}

fn render_inline(inlines: &[Inline], output: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Escape(text) => output.push_str(&escape_html(text)),
            Inline::Strong(inner) => {
                output.push_str("<strong>");
                render_inline(inner, output);
                output.push_str("</strong>");
            }
            Inline::Em(inner) => {
                output.push_str("<em>");
                render_inline(inner, output);
                output.push_str("</em>");
            }
            Inline::Codespan(text) => {
                output.push_str("<code>");
                output.push_str(&escape_html(text));
                output.push_str("</code>");
            }
            Inline::Link { text, href, .. } => {
                if let Some(url) = sanitize_url(href) {
                    let _ = write!(output, "<a href=\"{}\">", escape_html(&url));
                    render_inline(text, output);
                    output.push_str("</a>");
                } else {
                    render_inline(text, output);
                }
            }
            Inline::Br => output.push_str("<br>"),
            Inline::Del(inner) => {
                output.push_str("<del>");
                render_inline(inner, output);
                output.push_str("</del>");
            }
            // Raw HTML from a transcript is data, not markup. Escaping it is
            // important because messages can contain arbitrary user text.
            Inline::Html(raw) => output.push_str(&escape_html(raw)),
            Inline::Latex { text, .. } => {
                output.push_str("<span class=\"math\">");
                output.push_str(&escape_html(text));
                output.push_str("</span>");
            }
        }
    }
}

fn render_blocks(blocks: &[Block], output: &mut String) {
    for block in blocks {
        match block {
            Block::Heading { level, tokens } => {
                let level = (*level).clamp(1, 6);
                let _ = write!(output, "<h{level}>");
                render_inline(tokens, output);
                let _ = writeln!(output, "</h{level}>");
            }
            Block::Paragraph(tokens) | Block::Text(tokens) => {
                output.push_str("<p>");
                render_inline(tokens, output);
                output.push_str("</p>\n");
            }
            Block::LatexBlock { text, .. } => {
                output.push_str("<div class=\"math-block\"><code>");
                output.push_str(&escape_html(text));
                output.push_str("</code></div>\n");
            }
            Block::Code { lang, text, .. } => {
                if lang.is_empty() {
                    output.push_str("<pre><code>");
                } else {
                    let _ = write!(
                        output,
                        "<pre><code class=\"language-{}\">",
                        escape_html(lang)
                    );
                }
                output.push_str(&escape_html(text));
                output.push_str("</code></pre>\n");
            }
            Block::List(list) => render_list(list, output),
            Block::Table(table) => render_table(table, output),
            Block::Blockquote(inner) => {
                output.push_str("<blockquote>\n");
                render_blocks(inner, output);
                output.push_str("</blockquote>\n");
            }
            Block::Hr => output.push_str("<hr>\n"),
            // The parser deliberately exposes raw HTML tokens. Keep them
            // visible as text instead of allowing transcript HTML to execute.
            Block::Html(raw) => {
                output.push_str("<pre class=\"markdown-raw\">");
                output.push_str(&escape_html(raw));
                output.push_str("</pre>\n");
            }
            Block::Space => output.push('\n'),
        }
    }
}

fn render_list(list: &ListBlock, output: &mut String) {
    if list.ordered {
        let _ = writeln!(output, "<ol start=\"{}\">", list.start);
    } else {
        output.push_str("<ul>\n");
    }
    for item in &list.items {
        output.push_str("<li>");
        if item.task {
            let marker = if item.checked { "[x] " } else { "[ ] " };
            let _ = write!(output, "<span class=\"task-marker\">{marker}</span>");
        }
        render_blocks(&item.tokens, output);
        output.push_str("</li>\n");
    }
    if list.ordered {
        output.push_str("</ol>\n");
    } else {
        output.push_str("</ul>\n");
    }
}

fn render_table(table: &TableBlock, output: &mut String) {
    output.push_str("<table><thead><tr>");
    for cell in &table.header {
        output.push_str("<th>");
        render_inline(cell, output);
        output.push_str("</th>");
    }
    output.push_str("</tr></thead><tbody>\n");
    for row in &table.rows {
        output.push_str("<tr>");
        for cell in row {
            output.push_str("<td>");
            render_inline(cell, output);
            output.push_str("</td>");
        }
        output.push_str("</tr>\n");
    }
    output.push_str("</tbody></table>\n");
}

fn render_markdown(source: &str) -> String {
    let blocks = pi_tui::components::markdown::parse_markdown(source);
    let mut output = String::new();
    render_blocks(&blocks, &mut output);
    output
}

fn value_string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn message_content(message: &Value) -> Option<&Value> {
    message.get("content")
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Extract text blocks with the same concatenation used by the upstream tree
/// and search views. Message rendering intentionally uses `content_text`,
/// which inserts a newline between independent text blocks.
fn extract_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillBlock<'a> {
    name: &'a str,
    _location: &'a str,
    content: &'a str,
    user_message: Option<&'a str>,
}

/// Parse the structural wrapper emitted for `/skill` invocations. The
/// wrapper is transcript data, not user-authored Markdown; the exporter must
/// render its body and optional prompt as separate sibling blocks.
fn parse_skill_block(text: &str) -> Option<SkillBlock<'_>> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let name_end = rest.find("\" location=\"")?;
    let name = &rest[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = &rest[name_end + "\" location=\"".len()..];
    let location_end = rest.find("\">\n")?;
    let location = &rest[..location_end];
    if location.is_empty() {
        return None;
    }
    let body = &rest[location_end + "\">\n".len()..];
    let closing = "\n</skill>";
    let close_at = body.find(closing)?;
    let content = &body[..close_at];
    let suffix = &body[close_at + closing.len()..];
    let user_message = match suffix {
        "" => None,
        suffix => Some(suffix.strip_prefix("\n\n")?.trim()).filter(|text| !text.is_empty()),
    };
    if !suffix.is_empty() && !suffix.starts_with("\n\n") {
        return None;
    }
    Some(SkillBlock {
        name,
        _location: location,
        content,
        user_message,
    })
}

fn content_images(content: Option<&Value>) -> Vec<&Value> {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
                .collect()
        })
        .unwrap_or_default()
}

fn is_base64(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn render_image(image: &Value, class_name: &str) -> String {
    let mime = image
        .get("mimeType")
        .or_else(|| image.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    let data = image
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let safe_mime = mime.starts_with("image/")
        && mime[6..].chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        });
    if !safe_mime || !is_base64(data) {
        return "<span class=\"tool-error\">[invalid image data]</span>".to_string();
    }
    format!(
        "<img class=\"{}\" src=\"data:{};base64,{}\" alt=\"embedded session image\">",
        escape_html(class_name),
        escape_html(mime),
        escape_html(data)
    )
}

fn replace_tabs(value: &str) -> String {
    value.replace('\t', "   ")
}

fn shorten_path(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if let Some((user, remainder)) = rest.split_once('/') {
                let _ = user;
                return format!("~{remainder}");
            }
        }
    }
    path.to_string()
}

fn truncate_chars(value: &str, max_len: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_len).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn normalize_tree_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn json_string(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| json_string(value))
}

fn object_value<'a>(args: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    args.and_then(Value::as_object)
        .and_then(|object| object.get(key))
}

fn first_object_value<'a>(args: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object_value(args, key))
}

fn string_argument<'a>(args: Option<&'a Value>, keys: &[&str]) -> Result<Option<&'a str>, ()> {
    match first_object_value(args, keys) {
        None => Ok(None),
        Some(value) => value.as_str().map(Some).ok_or(()),
    }
}

fn display_argument(args: Option<&Value>, keys: &[&str], fallback: &str) -> String {
    match string_argument(args, keys) {
        Ok(Some(value)) => escape_html(&shorten_path(value)),
        Ok(None) => escape_html(fallback),
        Err(()) => "<span class=\"tool-error\">[invalid arg]</span>".to_string(),
    }
}

fn number_argument(args: Option<&Value>, key: &str) -> Option<String> {
    object_value(args, key).and_then(|value| {
        value
            .as_i64()
            .map(|number| number.to_string())
            .or_else(|| value.as_u64().map(|number| number.to_string()))
            .or_else(|| value.as_f64().map(|number| number.to_string()))
    })
}

fn format_tool_call_summary(call: &Value) -> String {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
    let args = call.get("arguments");
    match name {
        "read" => {
            let path = string_argument(args, &["path", "file_path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_default();
            let offset = number_argument(args, "offset");
            let limit = number_argument(args, "limit");
            let line_range = match (offset, limit) {
                (Some(start), Some(limit)) => format!(
                    ":{}-{}",
                    start,
                    start.parse::<i64>().unwrap_or(1) + limit.parse::<i64>().unwrap_or(1) - 1
                ),
                (Some(start), None) => format!(":{start}"),
                (None, Some(limit)) => format!(":1-{limit}"),
                (None, None) => String::new(),
            };
            format!("[read: {path}{line_range}]")
        }
        "write" => format!(
            "[write: {}]",
            string_argument(args, &["path", "file_path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_default()
        ),
        "edit" => format!(
            "[edit: {}]",
            string_argument(args, &["path", "file_path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_default()
        ),
        "bash" => {
            let command = string_argument(args, &["command"])
                .ok()
                .flatten()
                .unwrap_or_default();
            let command = normalize_tree_text(command);
            let suffix = if command.chars().count() > 50 {
                "..."
            } else {
                ""
            };
            format!("[bash: {}{suffix}]", truncate_chars(&command, 50))
        }
        "grep" => format!(
            "[grep: /{}/ in {}]",
            string_argument(args, &["pattern"])
                .ok()
                .flatten()
                .unwrap_or_default(),
            string_argument(args, &["path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_else(|| ".".to_string())
        ),
        "find" => format!(
            "[find: {} in {}]",
            string_argument(args, &["pattern"])
                .ok()
                .flatten()
                .unwrap_or_default(),
            string_argument(args, &["path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_else(|| ".".to_string())
        ),
        "ls" => format!(
            "[ls: {}]",
            string_argument(args, &["path"])
                .ok()
                .flatten()
                .map(shorten_path)
                .unwrap_or_else(|| ".".to_string())
        ),
        _ => {
            let args_json = args.map(json_string).unwrap_or_else(|| "{}".to_string());
            let suffix = if args_json.chars().count() > 40 {
                "..."
            } else {
                ""
            };
            format!("[{name}: {}{suffix}]", truncate_chars(&args_json, 40))
        }
    }
}

fn entry_id(entry: &Value, fallback: usize) -> String {
    entry
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("entry-{fallback}"))
}

fn timestamp_html(entry: &Value) -> String {
    entry
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|timestamp| {
            format!(
                "<div class=\"message-timestamp\">{}</div>",
                escape_html(timestamp)
            )
        })
        .unwrap_or_default()
}

fn message_role(entry: &Value) -> Option<&str> {
    entry
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
}

fn find_tool_result<'a>(entries: &'a [Value], tool_call_id: &str) -> Option<&'a Value> {
    entries.iter().find(|entry| {
        message_role(entry) == Some("toolResult")
            && entry
                .get("message")
                .and_then(|message| message.get("toolCallId"))
                .and_then(Value::as_str)
                == Some(tool_call_id)
    })
}

fn find_tool_call<'a>(entries: &'a [Value], tool_call_id: &str) -> Option<&'a Value> {
    entries.iter().find_map(|entry| {
        if message_role(entry) != Some("assistant") {
            return None;
        }
        entry
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks.iter().find(|block| {
                    block.get("type").and_then(Value::as_str) == Some("toolCall")
                        && block.get("id").and_then(Value::as_str) == Some(tool_call_id)
                })
            })
    })
}

fn result_text(entry: &Value) -> String {
    entry
        .get("message")
        .map(|message| content_text(message_content(message)))
        .unwrap_or_default()
}

fn result_images(entry: &Value) -> Vec<&Value> {
    entry
        .get("message")
        .and_then(message_content)
        .map(|content| content_images(Some(content)))
        .unwrap_or_default()
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
fn render_output(output: &str, language: Option<&str>) -> String {
    if language.filter(|language| !language.is_empty()).is_none() {
        // Plain output is line-oriented in the upstream template. Avoid
        // literal indentation/newlines in the generated HTML so whitespace in
        // the captured process output remains the only displayed whitespace.
        let mut html = String::from("<div class=\"tool-output\">");
        for line in replace_tabs(output).split('\n') {
            let _ = write!(html, "<div>{}</div>", escape_html(line));
        }
        html.push_str("</div>");
        return html;
    }

    let mut html = String::from("<div class=\"tool-output\"><pre><code");
    let language = language.expect("language was checked above");
    let _ = write!(html, " class=\"language-{}\"", escape_html(language));
    html.push('>');
    html.push_str(&escape_html(&replace_tabs(output)));
    html.push_str("</code></pre></div>");
    html
}

/// HTML returned by an extension renderer has already gone through its
/// ANSI/theme escaping pipeline. Preserve that markup, while removing only
/// the wrapper's leading/trailing layout whitespace.
fn render_pre_rendered_html(value: &str) -> String {
    value.trim().to_string()
}

fn language_from_path(path: Option<&str>) -> Option<&'static str> {
    let extension = path?.rsplit('.').next()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" => "html",
        "css" | "scss" => "css",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "md" => "markdown",
        _ => return None,
    })
}

fn render_tool_call(
    call: &Value,
    entries: &[Value],
    rendered_tools: Option<&Map<String, Value>>,
) -> String {
    let call_id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("tool-call");
    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
    let args = call.get("arguments");
    let result = find_tool_result(entries, call_id);
    let is_error = result
        .and_then(|entry| entry.get("message"))
        .and_then(|message| message.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if result.is_none() {
        "pending"
    } else if is_error {
        "error"
    } else {
        "success"
    };

    let mut html = format!(
        "<div class=\"tool-execution {status}\" data-tool-call-id=\"{}\">",
        escape_html(call_id)
    );
    match name {
        "bash" => {
            let command = match string_argument(args, &["command"]) {
                Ok(Some(command)) => escape_html(command),
                Ok(None) => "...".to_string(),
                Err(()) => "<span class=\"tool-error\">[invalid arg]</span>".to_string(),
            };
            let _ = write!(html, "<div class=\"tool-command\">$ {command}</div>");
            if let Some(result) = result {
                let output = result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&render_output(&output, None));
                }
            }
        }
        "read" => {
            let path = display_argument(args, &["file_path", "path"], "");
            html.push_str("<div class=\"tool-header\"><span class=\"tool-name\">read</span> ");
            html.push_str("<span class=\"tool-path\">");
            html.push_str(&path);
            if let Some(offset) = number_argument(args, "offset") {
                let _ = write!(html, "<span class=\"line-numbers\">:{offset}");
                if let Some(limit) = number_argument(args, "limit") {
                    let end =
                        offset.parse::<i64>().unwrap_or(1) + limit.parse::<i64>().unwrap_or(1) - 1;
                    let _ = write!(html, "-{end}");
                }
                html.push_str("</span>");
            }
            html.push_str("</span></div>");
            if let Some(result) = result {
                for image in result_images(result) {
                    html.push_str(&render_image(image, "tool-image"));
                }
                let output = result_text(result);
                if !output.is_empty() {
                    html.push_str(&render_output(
                        &output,
                        language_from_path(
                            string_argument(args, &["file_path", "path"]).ok().flatten(),
                        ),
                    ));
                }
            }
        }
        "write" => {
            let path = display_argument(args, &["file_path", "path"], "");
            html.push_str("<div class=\"tool-header\"><span class=\"tool-name\">write</span> ");
            let _ = write!(html, "<span class=\"tool-path\">{path}</span>");
            if let Ok(Some(content)) = string_argument(args, &["content"]) {
                let line_count = content.split('\n').count();
                if line_count > 10 {
                    let _ = write!(
                        html,
                        " <span class=\"line-count\">({line_count} lines)</span>"
                    );
                }
            }
            html.push_str("</div>");
            match string_argument(args, &["content"]) {
                Ok(Some(content)) if !content.is_empty() => html.push_str(&render_output(
                    content,
                    language_from_path(
                        string_argument(args, &["file_path", "path"]).ok().flatten(),
                    ),
                )),
                Ok(None) => {}
                Err(()) => html.push_str(
                    "<div class=\"tool-error\">[invalid content arg - expected string]</div>",
                ),
                _ => {}
            }
            if let Some(result) = result {
                let output = result_text(result).trim().to_string();
                if !output.trim().is_empty() {
                    html.push_str(&render_output(&output, None));
                }
            }
        }
        "edit" => {
            let path = display_argument(args, &["file_path", "path"], "");
            let _ = write!(
                html,
                "<div class=\"tool-header\"><span class=\"tool-name\">edit</span> <span class=\"tool-path\">{path}</span></div>"
            );
            if let Some(result) = result {
                if let Some(diff) = result
                    .get("message")
                    .and_then(|message| message.get("details"))
                    .and_then(|details| details.get("diff"))
                    .and_then(Value::as_str)
                {
                    html.push_str("<div class=\"tool-diff\">");
                    for line in diff.lines() {
                        let class_name = if line.starts_with('+') {
                            "diff-added"
                        } else if line.starts_with('-') {
                            "diff-removed"
                        } else {
                            "diff-context"
                        };
                        let _ = write!(
                            html,
                            "<div class=\"{class_name}\">{}</div>",
                            escape_html(&replace_tabs(line))
                        );
                    }
                    html.push_str("</div>");
                } else {
                    let output = result_text(result).trim().to_string();
                    if !output.trim().is_empty() {
                        html.push_str(&render_output(&output, None));
                    }
                }
            }
        }
        "ls" => {
            let path = display_argument(args, &["path"], ".");
            let _ = write!(
                html,
                "<div class=\"tool-header\"><span class=\"tool-name\">ls</span> <span class=\"tool-path\">{path}</span>"
            );
            if let Some(limit) = number_argument(args, "limit") {
                let _ = write!(
                    html,
                    " <span class=\"line-count\">(limit {})</span>",
                    escape_html(&limit)
                );
            }
            html.push_str("</div>");
            if let Some(result) = result {
                let output = result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&render_output(&output, None));
                }
            }
        }
        _ => {
            let mut rendered_result = false;
            if let Some(rendered) = rendered_tools
                .and_then(|tools| tools.get(call_id))
                .and_then(Value::as_object)
            {
                if let Some(value) = rendered.get("callHtml").and_then(Value::as_str) {
                    html.push_str("<div class=\"tool-header ansi-rendered\">");
                    html.push_str(&render_pre_rendered_html(value));
                    html.push_str("</div>");
                } else {
                    let _ = write!(
                        html,
                        "<div class=\"tool-header\"><span class=\"tool-name\">{}</span></div>",
                        escape_html(name)
                    );
                }
                let collapsed = rendered
                    .get("resultHtmlCollapsed")
                    .and_then(Value::as_str)
                    .map(render_pre_rendered_html);
                let expanded = rendered
                    .get("resultHtmlExpanded")
                    .and_then(Value::as_str)
                    .map(render_pre_rendered_html);
                match (collapsed, expanded) {
                    (Some(collapsed), Some(expanded)) if collapsed != expanded => {
                        rendered_result = true;
                        let _ = write!(
                            html,
                            "<div class=\"tool-output expandable ansi-rendered\"><div class=\"output-preview\">{collapsed}</div><div class=\"output-full\">{expanded}</div></div>"
                        );
                    }
                    (Some(_), Some(expanded)) | (None, Some(expanded)) => {
                        rendered_result = true;
                        let _ = write!(
                            html,
                            "<div class=\"tool-output ansi-rendered\">{expanded}</div>"
                        );
                    }
                    (Some(collapsed), None) => {
                        rendered_result = true;
                        let _ = write!(
                            html,
                            "<div class=\"tool-output ansi-rendered\">{collapsed}</div>"
                        );
                    }
                    (None, None) => {}
                }
            } else {
                let _ = write!(
                    html,
                    "<div class=\"tool-header\"><span class=\"tool-name\">{}</span></div>",
                    escape_html(name)
                );
                html.push_str(&render_output(
                    &json_pretty(args.unwrap_or(&Value::Null)),
                    None,
                ));
            }
            if !rendered_result {
                if let Some(result) = result {
                    let output = result_text(result).trim().to_string();
                    if !output.is_empty() {
                        html.push_str(&render_output(&output, None));
                    }
                }
            }
        }
    }
    html.push_str("</div>");
    html
}

fn render_tool_result_entry(_entry: &Value, _entries: &[Value]) -> String {
    // A matched result is rendered inside its assistant tool-call block. This
    // avoids duplicating output in the transcript, matching template.js.
    String::new()
}

fn render_message_entry(
    entry: &Value,
    entries: &[Value],
    rendered_tools: Option<&Map<String, Value>>,
    index: usize,
) -> String {
    let message = entry.get("message").unwrap_or(&Value::Null);
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let id = entry_id(entry, index);
    let id_attr = escape_html(&id);
    let timestamp = timestamp_html(entry);
    let content = message_content(message);
    let mut html = String::new();

    match role {
        "user" => {
            let text = content_text(content);
            if let Some(skill) = parse_skill_block(&text) {
                let mut html = format!(
                    "<div class=\"skill-user-entry\" id=\"entry-{id_attr}\">{timestamp}<div class=\"skill-invocation\">"
                );
                let _ = write!(
                    html,
                    "<div class=\"skill-invocation-label\">[skill] {}</div><div class=\"skill-invocation-collapsed\">{} (click to expand)</div><div class=\"skill-invocation-content markdown-content\">{}</div>",
                    escape_html(skill.name),
                    escape_html(skill.name),
                    render_markdown(skill.content)
                );
                html.push_str("</div>");
                let images = content_images(content);
                if skill.user_message.is_some() || !images.is_empty() {
                    html.push_str("<div class=\"user-message\">");
                    for image in images {
                        html.push_str(&render_image(image, "message-image"));
                    }
                    if let Some(user_message) = skill.user_message {
                        let _ = write!(
                            html,
                            "<div class=\"markdown-content\">{}</div>",
                            render_markdown(user_message)
                        );
                    }
                    html.push_str("</div>");
                }
                html.push_str("</div>");
                return html;
            }
            let _ = write!(
                html,
                "<article class=\"user-message\" id=\"entry-{id_attr}\">"
            );
            html.push_str(&timestamp);
            for image in content_images(content) {
                html.push_str(&render_image(image, "message-image"));
            }
            let text = content_text(content);
            if !text.trim().is_empty() {
                let _ = write!(
                    html,
                    "<div class=\"markdown-content\">{}</div>",
                    render_markdown(&text)
                );
            }
            html.push_str("</article>");
        }
        "assistant" => {
            let _ = write!(
                html,
                "<article class=\"assistant-message\" id=\"entry-{id_attr}\">"
            );
            html.push_str(&timestamp);
            if let Some(blocks) = content.and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    let _ = write!(
                                        html,
                                        "<div class=\"assistant-text markdown-content\">{}</div>",
                                        render_markdown(text)
                                    );
                                }
                            }
                        }
                        Some("thinking") => {
                            if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                                if !thinking.trim().is_empty() {
                                    let _ = write!(
                                        html,
                                        "<div class=\"thinking-block\"><div class=\"thinking-text\">{}</div></div>",
                                        escape_html(thinking)
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                        html.push_str(&render_tool_call(block, entries, rendered_tools));
                    }
                }
            } else {
                let text = content_text(content);
                if !text.trim().is_empty() {
                    let _ = write!(
                        html,
                        "<div class=\"assistant-text markdown-content\">{}</div>",
                        render_markdown(&text)
                    );
                }
            }
            match message.get("stopReason").and_then(Value::as_str) {
                Some("aborted") => html.push_str("<div class=\"error-text\">Aborted</div>"),
                Some("error") => {
                    let error = message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown error");
                    let _ = write!(
                        html,
                        "<div class=\"error-text\">Error: {}</div>",
                        escape_html(error)
                    );
                }
                _ => {}
            }
            html.push_str("</article>");
        }
        "bashExecution" => {
            let command = value_string(message.get("command")).unwrap_or_default();
            let is_error = message
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || message
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .is_some_and(|exit_code| exit_code != 0);
            let status = if is_error { "error" } else { "success" };
            let _ = write!(
                html,
                "<article class=\"tool-execution {status}\" id=\"entry-{id_attr}\">"
            );
            html.push_str(&timestamp);
            let _ = write!(
                html,
                "<div class=\"tool-command\">$ {}</div>",
                escape_html(command)
            );
            if let Some(output) = value_string(message.get("output")) {
                html.push_str(&render_output(output, None));
            }
            if message
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                html.push_str("<div class=\"error-text\">(cancelled)</div>");
            } else if let Some(exit_code) = message.get("exitCode").and_then(Value::as_i64) {
                if exit_code != 0 {
                    let _ = write!(html, "<div class=\"error-text\">(exit {exit_code})</div>");
                }
            }
            html.push_str("</article>");
        }
        "toolResult" => html.push_str(&render_tool_result_entry(entry, entries)),
        _ => {
            let _ = write!(
                html,
                "<article class=\"hook-message\" id=\"entry-{id_attr}\">{timestamp}<div class=\"hook-type\">[{}]</div><pre>{}</pre></article>",
                escape_html(role),
                escape_html(&json_pretty(message))
            );
        }
    }
    html
}

fn render_entry(
    entry: &Value,
    entries: &[Value],
    rendered_tools: Option<&Map<String, Value>>,
    index: usize,
) -> String {
    let id = entry_id(entry, index);
    let id_attr = escape_html(&id);
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => render_message_entry(entry, entries, rendered_tools, index),
        Some("model_change") => format!(
            "<article class=\"model-change\" id=\"entry-{id_attr}\">{}Switched to model: <span class=\"model-name\">{}/{}</span></article>",
            timestamp_html(entry),
            escape_html(entry.get("provider").and_then(Value::as_str).unwrap_or("unknown")),
            escape_html(entry.get("modelId").and_then(Value::as_str).unwrap_or("unknown"))
        ),
        Some("thinking_level_change") => format!(
            "<article class=\"model-change\" id=\"entry-{id_attr}\">{}Thinking level: <span class=\"model-name\">{}</span></article>",
            timestamp_html(entry),
            escape_html(entry.get("thinkingLevel").and_then(Value::as_str).unwrap_or("unknown"))
        ),
        Some("compaction") => format!(
            "<article class=\"compaction\" id=\"entry-{id_attr}\">{}<div class=\"compaction-label\">[compaction]</div><div class=\"compaction-content-static\"><strong>Compacted from {} tokens</strong><br><br>{}</div></article>",
            timestamp_html(entry),
            entry.get("tokensBefore").and_then(Value::as_u64).unwrap_or(0),
            escape_html(entry.get("summary").and_then(Value::as_str).unwrap_or_default())
        ),
        Some("branch_summary") => format!(
            "<article class=\"branch-summary\" id=\"entry-{id_attr}\">{}<div class=\"branch-summary-header\">Branch Summary</div><div class=\"markdown-content\">{}</div></article>",
            timestamp_html(entry),
            render_markdown(entry.get("summary").and_then(Value::as_str).unwrap_or_default())
        ),
        Some("custom_message") => {
            if !entry
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                return String::new();
            }
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| json_string(entry.get("content").unwrap_or(&Value::Null)));
            format!(
                "<article class=\"hook-message\" id=\"entry-{id_attr}\">{}<div class=\"hook-type\">[{}]</div><div class=\"markdown-content\">{}</div></article>",
                timestamp_html(entry),
                escape_html(entry.get("customType").and_then(Value::as_str).unwrap_or("custom")),
                render_markdown(&content)
            )
        }
        _ => format!(
            "<details class=\"raw-entry\" id=\"entry-{id_attr}\" open><summary>{}</summary><pre>{}</pre></details>",
            escape_html(entry.get("type").and_then(Value::as_str).unwrap_or("entry")),
            escape_html(&json_pretty(entry))
        ),
    }
}

#[derive(Default)]
struct ExportStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_results: usize,
    tool_calls: usize,
    custom_messages: usize,
    compactions: usize,
    branch_summaries: usize,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    input_cost: f64,
    output_cost: f64,
    cache_read_cost: f64,
    cache_write_cost: f64,
    models: Vec<String>,
}

fn u64_field(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .and_then(Value::as_i64)
                .and_then(|number| number.try_into().ok())
        })
        .unwrap_or(0)
}

fn f64_field(value: Option<&Value>) -> f64 {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_i64).map(|number| number as f64))
        .or_else(|| value.and_then(Value::as_u64).map(|number| number as f64))
        .unwrap_or(0.0)
}

fn compute_stats(entries: &[Value]) -> ExportStats {
    let mut stats = ExportStats::default();
    for entry in entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => match message_role(entry) {
                Some("user") => stats.user_messages += 1,
                Some("toolResult") => stats.tool_results += 1,
                Some("assistant") => {
                    stats.assistant_messages += 1;
                    if let Some(message) = entry.get("message") {
                        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                            stats.tool_calls += blocks
                                .iter()
                                .filter(|block| {
                                    block.get("type").and_then(Value::as_str) == Some("toolCall")
                                })
                                .count();
                        }
                        let model = message
                            .get("model")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        let provider = message.get("provider").and_then(Value::as_str);
                        if let Some(model) = model {
                            let model = provider
                                .map(|provider| format!("{provider}/{model}"))
                                .unwrap_or(model);
                            if !stats.models.contains(&model) {
                                stats.models.push(model);
                            }
                        }
                        if let Some(usage) = message.get("usage") {
                            stats.input_tokens += u64_field(usage.get("input"));
                            stats.output_tokens += u64_field(usage.get("output"));
                            stats.cache_read_tokens += u64_field(usage.get("cacheRead"));
                            stats.cache_write_tokens += u64_field(usage.get("cacheWrite"));
                            if let Some(cost) = usage.get("cost") {
                                stats.input_cost +=
                                    f64_field(cost.get("input").or_else(|| cost.get("inputCost")));
                                stats.output_cost += f64_field(
                                    cost.get("output").or_else(|| cost.get("outputCost")),
                                );
                                stats.cache_read_cost += f64_field(
                                    cost.get("cacheRead").or_else(|| cost.get("cache_read")),
                                );
                                stats.cache_write_cost += f64_field(
                                    cost.get("cacheWrite").or_else(|| cost.get("cache_write")),
                                );
                            }
                        }
                    }
                }
                _ => {}
            },
            Some("custom_message") => stats.custom_messages += 1,
            Some("compaction") => stats.compactions += 1,
            Some("branch_summary") => stats.branch_summaries += 1,
            _ => {}
        }
    }
    stats
}

fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=9_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

fn render_header(data: &SessionData) -> String {
    let header = &data.header;
    let stats = compute_stats(&data.entries);
    let session_id = header
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let timestamp = header
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let models = if stats.models.is_empty() {
        "unknown".to_string()
    } else {
        stats.models.join(", ")
    };
    let mut message_parts = Vec::new();
    if stats.user_messages > 0 {
        message_parts.push(format!("{} user", stats.user_messages));
    }
    if stats.assistant_messages > 0 {
        message_parts.push(format!("{} assistant", stats.assistant_messages));
    }
    if stats.tool_results > 0 {
        message_parts.push(format!("{} tool results", stats.tool_results));
    }
    if stats.custom_messages > 0 {
        message_parts.push(format!("{} custom", stats.custom_messages));
    }
    if stats.compactions > 0 {
        message_parts.push(format!("{} compactions", stats.compactions));
    }
    if stats.branch_summaries > 0 {
        message_parts.push(format!("{} branch summaries", stats.branch_summaries));
    }
    let message_summary = if message_parts.is_empty() {
        "0".to_string()
    } else {
        message_parts.join(", ")
    };
    let total_cost =
        stats.input_cost + stats.output_cost + stats.cache_read_cost + stats.cache_write_cost;

    let mut html = String::from("<section class=\"header session-header\">");
    let _ = write!(html, "<h1>Session: {}</h1>", escape_html(session_id));
    html.push_str("<div class=\"header-info\">");
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Date:</span><span class=\"info-value\">{}</span></div>",
        escape_html(timestamp)
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Working directory:</span><span class=\"info-value\">{}</span></div>",
        escape_html(cwd)
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Models:</span><span class=\"info-value\">{}</span></div>",
        escape_html(&models)
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Messages:</span><span class=\"info-value\">{message_summary}</span></div>"
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Tools:</span><span class=\"info-value\">{} calls</span></div>",
        stats.tool_calls
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Tokens:</span><span class=\"info-value\">in {}, out {}, cache read {}, cache write {}</span></div>",
        format_tokens(stats.input_tokens),
        format_tokens(stats.output_tokens),
        format_tokens(stats.cache_read_tokens),
        format_tokens(stats.cache_write_tokens)
    );
    let _ = write!(
        html,
        "<div class=\"info-item\"><span class=\"info-label\">Cost:</span><span class=\"info-value\">${total_cost:.3}</span></div>"
    );
    html.push_str("</div>");

    if let Some(system_prompt) = &data.system_prompt {
        html.push_str("<details class=\"system-prompt\" open><summary class=\"system-prompt-header\">System Prompt</summary><pre class=\"system-prompt-full-static\">");
        html.push_str(&escape_html(system_prompt));
        html.push_str("</pre></details>");
    }
    if let Some(tools) = &data.tools {
        html.push_str(
            "<section class=\"tools-list\"><div class=\"tools-header\">Available Tools</div>",
        );
        for tool in tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let _ = write!(
                html,
                "<div class=\"tool-item\"><span class=\"tool-item-name\">{}</span> - <span class=\"tool-item-desc\">{}</span>",
                escape_html(name),
                escape_html(description)
            );
            if let Some(parameters) = tool.get("parameters").or_else(|| tool.get("inputSchema")) {
                let _ = write!(
                    html,
                    "<pre class=\"tool-parameters\">{}</pre>",
                    escape_html(&json_pretty(parameters))
                );
            }
            html.push_str("</div>");
        }
        html.push_str("</section>");
    }
    let _ = write!(
        html,
        "<details class=\"session-header-raw\" open><summary>Raw session header</summary><pre>{}</pre></details>",
        escape_html(&json_pretty(header))
    );
    html.push_str("</section>");
    html
}

fn parent_id(entry: &Value) -> Option<&str> {
    entry.get("parentId").and_then(Value::as_str)
}

fn entry_depth(entry: &Value, entries: &[Value]) -> usize {
    let mut depth = 0;
    let mut current = parent_id(entry);
    let mut seen = HashSet::new();
    while let Some(parent) = current {
        if !seen.insert(parent) {
            break;
        }
        depth += 1;
        current = entries
            .iter()
            .find(|candidate| entry_id(candidate, 0) == parent)
            .and_then(parent_id);
        if depth >= entries.len() {
            break;
        }
    }
    depth.min(12)
}

fn active_path_ids(entries: &[Value], leaf_id: Option<&str>) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut current = leaf_id.map(ToOwned::to_owned);
    while let Some(id) = current {
        if !ids.insert(id.clone()) {
            break;
        }
        current = entries
            .iter()
            .find(|entry| entry_id(entry, 0) == id)
            .and_then(parent_id)
            .map(ToOwned::to_owned);
    }
    ids
}

fn label_for_entry(entries: &[Value], id: &str) -> Option<String> {
    let mut label = None;
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("label")
            || entry.get("targetId").and_then(Value::as_str) != Some(id)
        {
            continue;
        }
        label = entry
            .get("label")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
    }
    label
}

fn tree_text(entry: &Value, entries: &[Value]) -> String {
    let label = label_for_entry(entries, &entry_id(entry, 0))
        .map(|label| format!("[{label}] "))
        .unwrap_or_default();
    let text = match entry.get("type").and_then(Value::as_str) {
        Some("message") => match message_role(entry) {
            Some("user") => {
                let content = entry.get("message").and_then(message_content);
                let raw_text = extract_content(content);
                if let Some(skill) = parse_skill_block(&raw_text) {
                    let user_message = skill
                        .user_message
                        .map(|text| {
                            format!(
                                " · user: {}",
                                truncate_chars(&normalize_tree_text(text), 100)
                            )
                        })
                        .unwrap_or_default();
                    format!("skill: {}{user_message}", truncate_chars(skill.name, 100))
                } else {
                    format!(
                        "user: {}",
                        truncate_chars(&normalize_tree_text(&raw_text), 100)
                    )
                }
            }
            Some("assistant") => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                let content = extract_content(message_content(message));
                if content.trim().is_empty() {
                    match message.get("stopReason").and_then(Value::as_str) {
                        Some("aborted") => "assistant: (aborted)".to_string(),
                        Some("error") => format!(
                            "assistant: {}",
                            truncate_chars(
                                message
                                    .get("errorMessage")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Unknown error"),
                                100
                            )
                        ),
                        _ => "assistant: (no text)".to_string(),
                    }
                } else {
                    format!(
                        "assistant: {}",
                        truncate_chars(&normalize_tree_text(&content), 100)
                    )
                }
            }
            Some("toolResult") => {
                let call_id = entry
                    .get("message")
                    .and_then(|message| message.get("toolCallId"))
                    .and_then(Value::as_str);
                find_tool_call(entries, call_id.unwrap_or(""))
                    .map(format_tool_call_summary)
                    .unwrap_or_else(|| "[tool result]".to_string())
            }
            Some("bashExecution") => format!(
                "[bash]: {}",
                truncate_chars(
                    &normalize_tree_text(
                        entry
                            .get("message")
                            .and_then(|message| value_string(message.get("command")))
                            .unwrap_or_default()
                    ),
                    100
                )
            ),
            Some(role) => format!("[{role}]"),
            None => "[message]".to_string(),
        },
        Some("compaction") => format!(
            "[compaction: {}k tokens]",
            u64_field(entry.get("tokensBefore")) / 1_000
        ),
        Some("branch_summary") => format!(
            "[branch summary]: {}",
            truncate_chars(
                &normalize_tree_text(
                    entry
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
                100
            )
        ),
        Some("custom_message") => format!(
            "[{}]: {}",
            entry
                .get("customType")
                .and_then(Value::as_str)
                .unwrap_or("custom"),
            truncate_chars(
                &normalize_tree_text(
                    entry
                        .get("content")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| { extract_content(entry.get("content")) })
                        .as_str(),
                ),
                100
            )
        ),
        Some("model_change") => format!(
            "[model: {}]",
            entry
                .get("modelId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        Some("thinking_level_change") => format!(
            "[thinking: {}]",
            entry
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        Some(other) => format!("[{other}]"),
        None => "[entry]".to_string(),
    };
    format!("{label}{text}")
}

fn render_tree(data: &SessionData) -> String {
    let active_ids = active_path_ids(&data.entries, data.leaf_id.as_deref());
    let mut html = String::new();
    for (index, entry) in data.entries.iter().enumerate() {
        let id = entry_id(entry, index);
        let depth = entry_depth(entry, &data.entries);
        let active = active_ids.contains(&id);
        let _ = write!(
            html,
            "<div class=\"tree-node{}\" data-entry-id=\"{}\"><span class=\"tree-prefix\">{}{}</span><span class=\"tree-marker\">{}</span><span class=\"tree-content\">{}</span></div>",
            if active { " in-path active" } else { "" },
            escape_html(&id),
            "  ".repeat(depth),
            if depth == 0 { "" } else { "└─ " },
            if active { "•" } else { " " },
            escape_html(&tree_text(entry, &data.entries))
        );
    }
    html
}

/// Generate the self-contained zero-JavaScript HTML export.
pub fn generate_html(data: &SessionData, theme_name: Option<&str>) -> Result<String, ExportError> {
    let theme_vars = generate_theme_vars(theme_name)?;
    let colors = theme::get_resolved_theme_colors(theme_name)
        .map_err(|e| ExportError::msg(format!("Failed to resolve theme colors: {e}")))?;
    let theme_export = theme::get_theme_export_colors(theme_name).unwrap_or_default();
    let derived = derive_export_colors(
        colors
            .get("userMessageBg")
            .map(String::as_str)
            .unwrap_or("#343541"),
    );
    let body_bg = theme_export
        .page_bg
        .clone()
        .unwrap_or_else(|| derived.0.clone());
    let container_bg = theme_export
        .card_bg
        .clone()
        .unwrap_or_else(|| derived.1.clone());
    let info_bg = theme_export
        .info_bg
        .clone()
        .unwrap_or_else(|| derived.2.clone());

    let css = TEMPLATE_CSS
        .replace("{{THEME_VARS}}", &theme_vars)
        .replace("{{BODY_BG}}", &body_bg)
        .replace("{{CONTAINER_BG}}", &container_bg)
        .replace("{{INFO_BG}}", &info_bg);
    let tree = render_tree(data);
    let entries = data
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            render_entry(entry, &data.entries, data.rendered_tools.as_ref(), index)
        })
        .collect::<String>();
    let title = format!(
        "{} session export",
        data.header
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    let mut html = TEMPLATE_HTML
        .replace("{{TITLE}}", &escape_html(&title))
        .replace("{{CSS}}", &css)
        .replace("{{HEADER}}", &render_header(data))
        .replace("{{TREE}}", &tree)
        .replace(
            "{{TREE_STATUS}}",
            &format!("{} entries", data.entries.len()),
        )
        .replace("{{ENTRIES}}", &entries);
    html = html.replace(
        "{{FOOTER}}",
        "Generated by pi-rust · static zero-JavaScript export",
    );
    Ok(html)
}

// ---------------------------------------------------------------------------
// Session file loading (mirrors SessionManager.open + getHeader/getEntries)
// ---------------------------------------------------------------------------

/// A session file's header, entries, and leaf id (as parsed for export).
pub struct LoadedSession {
    pub header: Option<Value>,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
}

/// Parse a session JSONL file.
/// Mirrors upstream `loadEntriesFromFile` + index building over the union of
/// the two session header formats this build produces:
/// - coding-agent format: first record `{"type":"session", ...}` (upstream's
///   own session files; `exportFromFile` parity).
/// - pi-agent v4 format: first record `{"kind":"header", "createdAt": ...}`
///   (the RPC/interactive session repo format). A `timestamp` field is
///   synthesized from `createdAt` for the HTML viewer's date display.
///
/// If the file is empty or its first parsed record is not a valid header, all
/// entries are dropped and `(None, [], None)` is returned — exactly like
/// upstream `loadEntriesFromFile`, which returns `[]` for a headerless file.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub fn load_session_file(path: &str) -> Result<LoadedSession, ExportError> {
    // Node's StringDecoder replaces malformed UTF-8 while reading JSONL. Use
    // the same loss-tolerant boundary so a bad auxiliary line does not make a
    // valid session unexportable.
    let bytes = std::fs::read(path)?;
    let content = String::from_utf8_lossy(&bytes);
    let mut file_entries: Vec<Value> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            file_entries.push(entry);
        }
    }
    if file_entries.is_empty() {
        return Ok(LoadedSession {
            header: None,
            entries: Vec::new(),
            leaf_id: None,
        });
    }
    let first = &file_entries[0];
    let session_type = first.get("type").and_then(|t| t.as_str());
    let kind = first.get("kind").and_then(|t| t.as_str());
    let valid_header = match (session_type, kind) {
        (Some("session"), _) => first.get("id").and_then(|i| i.as_str()).is_some(),
        (_, Some("header")) => first.get("id").and_then(|i| i.as_str()).is_some(),
        _ => false,
    };
    if !valid_header {
        return Ok(LoadedSession {
            header: None,
            entries: Vec::new(),
            leaf_id: None,
        });
    }
    let mut header = first.clone();
    // Synthesize `timestamp` from pi-agent v4 `createdAt` (ms epoch) so the
    // viewer's date shows for RPC/interactive session files.
    if session_type != Some("session") && header.get("timestamp").is_none() {
        if let Some(created) = header.get("createdAt").and_then(|c| c.as_u64()) {
            let secs = created / 1000;
            let millis = (created % 1000) as u32 * 1_000_000;
            let ts = time_from_epoch_ms(secs, millis);
            header
                .as_object_mut()
                .expect("header is an object")
                .insert("timestamp".to_string(), Value::String(ts));
        }
    }
    let entries: Vec<Value> = file_entries
        .iter()
        .filter(|e| {
            let t = e.get("type").and_then(|t| t.as_str());
            let k = e.get("kind").and_then(|t| t.as_str());
            !(t == Some("session") || k == Some("header"))
        })
        .cloned()
        .collect();
    let leaf_id = entries
        .last()
        .and_then(|e| e.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()));
    Ok(LoadedSession {
        header: Some(header),
        entries,
        leaf_id,
    })
}

/// Format an epoch (seconds, nanos) as an ISO-8601 UTC timestamp (used to
/// synthesize the HTML viewer's `timestamp` for pi-agent v4 headers).
pub(crate) fn time_from_epoch_ms(secs: u64, nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days as i64);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        hh,
        mm,
        ss,
        nanos / 1_000_000
    )
}

/// Format a millisecond epoch as the ISO-8601 timestamp used by v3 session
/// headers at the JSON output boundary.
pub(crate) fn iso8601_timestamp_from_epoch_ms(epoch_ms: u64) -> String {
    time_from_epoch_ms(epoch_ms / 1_000, ((epoch_ms % 1_000) * 1_000_000) as u32)
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Which tools are pre-rendered (upstream `TEMPLATE_RENDERED_TOOLS`).
pub fn is_template_rendered_tool(name: &str) -> bool {
    TEMPLATE_RENDERED_TOOLS.contains(&name)
}

// ---------------------------------------------------------------------------
// Export entry points
// ---------------------------------------------------------------------------

/// Export a session JSONL file to HTML (upstream `exportFromFile`).
pub fn export_session_file(
    input_path: &str,
    output_path: Option<&str>,
    theme_name: Option<&str>,
) -> Result<String, ExportError> {
    let resolved_input = Path::new(input_path);
    if !resolved_input.exists() {
        return Err(ExportError::msg(format!("File not found: {input_path}")));
    }
    let loaded = load_session_file(input_path)?;
    // `SessionManager.open` rejects a non-empty existing file whose parsed
    // records do not begin with a valid session header. Keep the loader
    // tolerant for callers that only need to inspect parsed entries, but do
    // not turn an invalid export source into a misleading empty transcript.
    if loaded.header.is_none() && std::fs::metadata(resolved_input)?.len() > 0 {
        return Err(ExportError::msg(format!(
            "Session file is not a valid {APP_NAME} session: {input_path}"
        )));
    }
    let data = SessionData {
        header: loaded
            .header
            .unwrap_or_else(|| serde_json::json!({"type": "session"})),
        entries: loaded.entries,
        leaf_id: loaded.leaf_id,
        system_prompt: None,
        tools: None,
        rendered_tools: None,
    };
    let html = generate_html(&data, theme_name)?;
    let output_path = default_output_path(input_path, output_path);
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

/// Compute the output path: explicit path if given, else
/// `<APP_NAME>-session-<basename-without-jsonl>.html` in the cwd.
fn default_output_path(input_path: &str, output_path: Option<&str>) -> String {
    if let Some(p) = output_path {
        let expanded = crate::config::expand_tilde_path(p);
        if expanded.starts_with("file://") {
            if let Ok(url) = url::Url::parse(&expanded) {
                if let Ok(path) = url.to_file_path() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
        return expanded;
    }
    let basename = Path::new(input_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    format!("{APP_NAME}-session-{basename}.html")
}

/// Pre-render custom tool calls/results using the callback-compatible renderer
/// above, with a safe generic fallback when no extension renderer is present.
pub fn pre_render_custom_tools_with_renderers<Call, Result>(
    entries: &[Value],
    mut render_call: Call,
    mut render_result: Result,
) -> Map<String, Value>
where
    Call: FnMut(&str, &str, &Value) -> Option<String>,
    Result: FnMut(&str, &str, &Value, &Value, bool) -> Option<(Option<String>, Option<String>)>,
{
    let mut rendered_tools = Map::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                        continue;
                    }
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    if id.is_empty() || is_template_rendered_tool(name) {
                        continue;
                    }
                    if let Some(call_html) =
                        render_call(id, name, block.get("arguments").unwrap_or(&Value::Null))
                    {
                        rendered_tools
                            .insert(id.to_string(), serde_json::json!({ "callHtml": call_html }));
                    }
                }
            }
            Some("toolResult") => {
                let id = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                let name = message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                if !rendered_tools.contains_key(id) && is_template_rendered_tool(name) {
                    continue;
                }
                let content = message.get("content").unwrap_or(&Value::Null);
                let details = message.get("details").unwrap_or(&Value::Null);
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let Some((collapsed, expanded)) =
                    render_result(id, name, content, details, is_error)
                else {
                    continue;
                };
                let rendered = rendered_tools
                    .entry(id.to_string())
                    .or_insert_with(|| serde_json::json!({}));
                let Some(object) = rendered.as_object_mut() else {
                    continue;
                };
                if let Some(collapsed) = collapsed {
                    object.insert("resultHtmlCollapsed".to_string(), Value::String(collapsed));
                }
                if let Some(expanded) = expanded {
                    object.insert("resultHtmlExpanded".to_string(), Value::String(expanded));
                }
            }
            _ => {}
        }
    }
    rendered_tools
}

pub fn pre_render_custom_tools(entries: &[Value]) -> Map<String, Value> {
    pre_render_custom_tools_with_renderers(
        entries,
        |_id, name, args| {
            Some(format!(
                "<span class=\"tool-name\">{}</span><pre>{}</pre>",
                escape_html(name),
                escape_html(&json_pretty(args))
            ))
        },
        |_id, _name, content, _details, is_error| {
            let output = content_text(Some(content)).trim().to_string();
            if output.is_empty() {
                return Some((None, None));
            }
            let class_name = if is_error { "error" } else { "success" };
            let mut html = format!("<div class=\"custom-result {class_name}\">");
            for line in replace_tabs(&output).split('\n') {
                let _ = write!(html, "<div>{}</div>", escape_html(line));
            }
            html.push_str("</div>");
            Some((None, Some(html)))
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn write_tmp_session(path: &Path) -> String {
        let header = r#"{"type":"session","version":4,"id":"sess_001","timestamp":"2026-08-22T00:00:00.000Z","cwd":"/tmp"}"#;
        let e1 = r#"{"type":"message","id":"msg_1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        let e2 = r#"{"type":"message","id":"msg_2","parentId":"msg_1","timestamp":"2026-08-22T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#;
        let content = format!("{header}\n{e1}\n{e2}\n");
        std::fs::write(path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn derives_export_colors_dark_and_light() {
        let (page, card, info) = derive_export_colors("#343541");
        assert_eq!(page, "rgb(36, 37, 46)");
        assert_eq!(card, "rgb(44, 45, 55)");
        assert_eq!(info, "rgb(72, 68, 65)");
        // Parse-error fallback
        let (page2, card2, info2) = derive_export_colors("nonsense");
        assert_eq!(page2, "rgb(24, 24, 30)");
        assert_eq!(card2, "rgb(30, 30, 36)");
        assert_eq!(info2, "rgb(60, 55, 40)");
    }

    #[test]
    fn adjusts_brightness() {
        assert_eq!(adjust_brightness("#000000", 0.7), "rgb(0, 0, 0)");
        assert_eq!(adjust_brightness("#ffffff", 0.5), "rgb(128, 128, 128)");
        assert_eq!(adjust_brightness("not-a-color", 0.5), "not-a-color");
    }

    #[test]
    fn parses_upstream_rgb_whitespace_forms() {
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some((1, 2, 3)));
        assert_eq!(parse_color("rgb ( 1 , 2 , 3 )"), Some((1, 2, 3)));
        assert_eq!(parse_color("rgb( 1,\t2, 3 )"), Some((1, 2, 3)));
    }

    #[test]
    fn theme_vars_have_export_colors() {
        let vars = generate_theme_vars(Some("dark")).unwrap();
        assert!(vars.contains("--accent: #8abeb7;"));
        assert!(vars.contains("--exportPageBg: #18181e;"));
        assert!(vars.contains("--exportCardBg: #1e1e24;"));
        assert!(vars.contains("--exportInfoBg: #3c3728;"));
        let vars_light = generate_theme_vars(Some("light")).unwrap();
        assert!(vars_light.contains("--userMessageBg: #e8e8e8;"));
    }

    #[test]
    fn loads_session_file() {
        let dir =
            std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let path_str = write_tmp_session(&path);
        let loaded = load_session_file(&path_str).unwrap();
        let header = loaded.header.unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.leaf_id.as_deref(), Some("msg_2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_fails() {
        let err = export_session_file("/definitely/not/here.jsonl", None, None).unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[test]
    fn exports_pi_agent_v4_header_session() {
        let dir =
            std::env::temp_dir().join(format!("pi-export-v4-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let content = r#"{"kind":"header","version":4,"id":"sess_001","createdAt":1787375500419,"cwd":"/tmp"}
{"type":"message","id":"msg_1","parentId":null,"timestamp":"2026-08-22T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"#;
        std::fs::write(&path, content).unwrap();
        let loaded = load_session_file(path.to_str().unwrap()).unwrap();
        let header = loaded.header.unwrap();
        assert_eq!(header["kind"], "header");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.leaf_id.as_deref(), Some("msg_1"));
        // timestamp synthesized from createdAt
        assert_eq!(header["timestamp"], "2026-08-22T05:11:40.419Z");
        let out = dir.join("out.html");
        let out_str = export_session_file(
            path.to_str().unwrap(),
            Some(out.to_str().unwrap()),
            Some("dark"),
        )
        .unwrap();
        let html = std::fs::read_to_string(&out_str).unwrap();
        assert!(html.contains("Session: sess_001"));
        assert!(html.contains("2026-08-22T05:11:40.419Z"));
        assert!(html.contains("hi"));
        assert!(!html.contains("<script"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exports_html_with_theme() {
        let dir =
            std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let path_str = write_tmp_session(&path);
        let out = dir.join("out.html");
        let out_str =
            export_session_file(&path_str, Some(out.to_str().unwrap()), Some("dark")).unwrap();
        let html = std::fs::read_to_string(&out_str).unwrap();
        for marker in [
            "{{CSS}}",
            "{{TITLE}}",
            "{{HEADER}}",
            "{{TREE}}",
            "{{TREE_STATUS}}",
            "{{ENTRIES}}",
            "{{FOOTER}}",
        ] {
            assert!(!html.contains(marker), "unsubstituted marker {marker}");
        }
        assert!(html.contains("--accent: #8abeb7;"));
        assert!(html.contains("--exportPageBg: #18181e;"));
        assert!(html.contains("class=\"user-message\""));
        assert!(html.contains("class=\"assistant-message\""));
        assert!(html.contains("hello"));
        assert!(html.contains("hi there"));
        assert!(html.contains("Static zero-JavaScript export"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("marked.min.js"));
        assert!(!html.contains("highlight.min.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_export_renders_tools_and_escapes_transcript_markup() {
        let data = SessionData {
            header: serde_json::json!({
                "type": "session",
                "id": "safe-session",
                "timestamp": "2026-08-22T00:00:00.000Z",
                "cwd": "/tmp"
            }),
            entries: vec![
                serde_json::json!({
                    "type": "message",
                    "id": "user-1",
                    "parentId": null,
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "<script>alert('x')</script> [unsafe](javascript:alert(1))"}]
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant-1",
                    "parentId": "user-1",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "toolCall",
                            "id": "call-1",
                            "name": "bash",
                            "arguments": {"command": "printf '<owned>'"}
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "result-1",
                    "parentId": "assistant-1",
                    "message": {
                        "role": "toolResult",
                        "toolCallId": "call-1",
                        "content": [{"type": "text", "text": "<owned>"}],
                        "isError": false
                    }
                }),
            ],
            leaf_id: Some("result-1".to_string()),
            system_prompt: None,
            tools: None,
            rendered_tools: None,
        };
        let html = generate_html(&data, Some("dark")).unwrap();
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
        assert!(html.contains("printf &#39;&lt;owned&gt;&#39;"));
        assert!(html.contains("&lt;owned&gt;"));
        assert!(html.contains("tool-result-reference"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn default_output_name() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir =
            std::env::temp_dir().join(format!("pi-export-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-08-22_abc123.jsonl");
        let path_str = write_tmp_session(&path);
        // chdir so the default resolves deterministically
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let out = export_session_file(&path_str, None, Some("dark")).unwrap();
        assert!(out.ends_with("pi-session-2026-08-22_abc123.html"));
        assert!(Path::new(&out).exists());
        std::env::set_current_dir(cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_replace_semantics_match_es() {
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$&"), "A{{M}}B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$$"), "A$B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$1"), "A$1B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$0"), "A$0B");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$`"), "AAB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$'"), "ABB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "x$1y"), "Ax$1yB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "no-dollar"), "Ano-dollarB");
        assert_eq!(js_replace("A{{M}}B", "{{M}}", "$10"), "A$10B");
        // Only first occurrence is replaced (JS behavior)
        assert_eq!(js_replace("{{M}}x{{M}}", "{{M}}", "R"), "Rx{{M}}");
        // No marker present -> unchanged
        assert_eq!(js_replace("hello", "{{M}}", "R"), "hello");
    }

    #[test]
    fn template_rendered_tools_match() {
        assert!(is_template_rendered_tool("bash"));
        assert!(is_template_rendered_tool("edit"));
        assert!(!is_template_rendered_tool("custom_tool"));
    }

    #[test]
    fn content_text_ignores_text_fields_on_non_text_blocks() {
        let content = serde_json::json!([
            {"type": "toolCall", "text": "must not become user text"},
            {"type": "text", "text": "first"},
            {"type": "image", "text": "must also be ignored"},
            {"type": "text", "text": "second"}
        ]);
        assert_eq!(content_text(Some(&content)), "first\nsecond");
    }

    #[test]
    fn header_reports_non_message_stats_and_total_cost() {
        let data = SessionData {
            header: serde_json::json!({
                "type": "session",
                "id": "stats-session",
                "timestamp": "2026-08-22T00:00:00.000Z",
                "cwd": "/tmp"
            }),
            entries: vec![
                serde_json::json!({
                    "type": "message",
                    "id": "assistant-1",
                    "message": {
                        "role": "assistant",
                        "provider": "faux",
                        "model": "fixture",
                        "content": [],
                        "usage": {
                            "input": 3,
                            "output": 2,
                            "cacheRead": 1,
                            "cacheWrite": 0,
                            "cost": {
                                "input": 0.1,
                                "output": 0.02,
                                "cacheRead": 0.003,
                                "cacheWrite": 0.002
                            }
                        }
                    }
                }),
                serde_json::json!({
                    "type": "custom_message",
                    "customType": "fixture",
                    "content": "custom"
                }),
                serde_json::json!({
                    "type": "compaction",
                    "tokensBefore": 100,
                    "summary": "summary"
                }),
                serde_json::json!({
                    "type": "branch_summary",
                    "summary": "branch"
                }),
            ],
            leaf_id: None,
            system_prompt: None,
            tools: None,
            rendered_tools: None,
        };
        let header = render_header(&data);
        assert!(header.contains("1 custom"));
        assert!(header.contains("1 compactions"));
        assert!(header.contains("1 branch summaries"));
        assert!(header.contains("$0.125"));
    }
}
