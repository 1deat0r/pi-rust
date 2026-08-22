//! TUI theme painting — the ANSI side of
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts`.
//!
//! Resolves theme colors through `crate::theme` (the JSON/color layer ported
//! by the parent) and paints ANSI truecolor fg/bg sequences. Falls back to
//! uncolored text when a color is unknown.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::theme;
use indexmap::IndexMap;

static THEME_STATE: RwLock<Option<ThemeState>> = RwLock::new(None);

#[derive(Debug, Clone)]
struct ThemeState {
    name: String,
    colors: HashMap<String, String>,
}

/// Resolve and cache the active theme's colors.
pub fn load_theme(name: &str) {
    let colors: IndexMap<String, String> = theme::get_resolved_theme_colors(Some(name)).unwrap_or_default();
    let colors: HashMap<String, String> = colors.into_iter().collect();
    let mut guard = THEME_STATE.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(ThemeState { name: name.to_string(), colors });
}

pub fn active_theme_name() -> Option<String> {
    THEME_STATE.read().ok().and_then(|g| g.as_ref().map(|s| s.name.clone()))
}

fn color_value(name: &str) -> Option<String> {
    let guard = THEME_STATE.read().ok()?;
    let state = guard.as_ref()?;
    state.colors.get(name).cloned()
}

/// Resolve a color name to an RGB triple if known.
fn color_rgb(name: &str) -> Option<(u8, u8, u8)> {
    let value = color_value(name)?;
    let hex = value.trim_start_matches('#');
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    None
}

/// Wrap text with a truecolor foreground for a theme color name.
pub fn fg(name: &str, text: impl AsRef<str>) -> String {
    match color_rgb(name) {
        Some((r, g, b)) => format!("\x1b[38;2;{r};{g};{b}m{}\x1b[39m", text.as_ref()),
        None => text.as_ref().to_string(),
    }
}

/// Wrap text with a truecolor background for a theme color name.
pub fn bg(name: &str, text: impl AsRef<str>) -> String {
    match color_rgb(name) {
        Some((r, g, b)) => format!("\x1b[48;2;{r};{g};{b}m{}\x1b[49m", text.as_ref()),
        None => text.as_ref().to_string(),
    }
}

/// Bold text.
pub fn bold(text: impl AsRef<str>) -> String {
    format!("\x1b[1m{}\x1b[22m", text.as_ref())
}

/// Italic text.
pub fn italic(text: impl AsRef<str>) -> String {
    format!("\x1b[3m{}\x1b[23m", text.as_ref())
}

/// Underlined text.
pub fn underline(text: impl AsRef<str>) -> String {
    format!("\x1b[4m{}\x1b[24m", text.as_ref())
}

/// Strikethrough text.
pub fn strikethrough(text: impl AsRef<str>) -> String {
    format!("\x1b[9m{}\x1b[29m", text.as_ref())
}

/// Inverse video.
pub fn inverse(text: impl AsRef<str>) -> String {
    format!("\x1b[7m{}\x1b[27m", text.as_ref())
}

/// Dim text.
pub fn dim(text: impl AsRef<str>) -> String {
    format!("\x1b[2m{}\x1b[22m", text.as_ref())
}

/// Build the Markdown theme from the resolved colors.
pub fn markdown_theme() -> pi_tui::components::markdown::MarkdownTheme {
    use pi_tui::components::markdown::MarkdownTheme;
    // Closures capture `fg`/`bg` helpers by reference via the closure itself.
    MarkdownTheme {
        heading: Box::new(|s| fg("headingText", s)),
        link: Box::new(|s| fg("accent", s)),
        link_url: Box::new(|s| fg("muted", s)),
        code: Box::new(|s| fg("codeText", s)),
        code_block: Box::new(|s| fg("codeBlockText", s)),
        code_block_border: Box::new(|s| fg("codeBlockBorder", s)),
        quote: Box::new(|s| fg("quoteText", s)),
        quote_border: Box::new(|s| fg("quoteBorder", s)),
        hr: Box::new(|s| fg("muted", s)),
        list_bullet: Box::new(|s| fg("accent", s)),
        bold: Box::new(|s| format!("\x1b[1m{s}\x1b[22m")),
        italic: Box::new(|s| format!("\x1b[3m{s}\x1b[23m")),
        strikethrough: Box::new(|s| format!("\x1b[9m{s}\x1b[29m")),
        underline: Box::new(|s| format!("\x1b[4m{s}\x1b[24m")),
        highlight_code: None,
        code_block_indent: Some("  ".to_string()),
    }
}

/// Default style for user message text (renders pre-inserted ANSI as-is).
pub fn user_message_style() -> pi_tui::components::markdown::DefaultTextStyle {
    use pi_tui::components::markdown::DefaultTextStyle;
    let color: Box<dyn Fn(&str) -> String + Send + Sync> = Box::new(|s| fg("userMessageText", s));
    DefaultTextStyle { color: Some(color), bg_color: None, bold: false, italic: false, strikethrough: false, underline: false }
}

/// Editor border color function.
pub fn editor_border() -> std::sync::Arc<dyn Fn(&str) -> String + Send + Sync> {
    std::sync::Arc::new(|s| fg("editorForeground", s))
}

/// Default style color fn for assistant thinking text.
pub fn thinking_color() -> Box<dyn Fn(&str) -> String + Send + Sync> {
    Box::new(|s| fg("thinkingText", s))
}
