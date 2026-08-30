//! Authentication selectors and login dialog used by interactive `/login`.
//!
//! The provider implementations remain owned by `pi-ai`.  This module only
//! owns the presentation and key handling that the interactive mode projects
//! into the editor slot while an auth flow is live.

use pi_ai::auth::AuthPrompt;
use pi_tui::components::Input;
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keys::{parse_key, TuiKey};
use pi_tui::tui::Component;
use pi_tui::utils::truncate_to_width;

use crate::interactive::tui_theme as t;

const MIN_WIDTH: usize = 24;
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Result of dispatching one key to an auth surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSurfaceAction {
    None,
    Submit(String),
    Cancel,
}

#[derive(Debug, Clone)]
struct AuthSelector {
    title: String,
    options: Vec<pi_ai::auth::AuthSelectOption>,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    query: String,
    searchable: bool,
    paste_mode: bool,
    paste_buffer: String,
    pending_start: String,
}

impl AuthSelector {
    fn new(title: String, options: Vec<pi_ai::auth::AuthSelectOption>, searchable: bool) -> Self {
        let mut selector = Self {
            title,
            options,
            filtered_indices: Vec::new(),
            selected_index: 0,
            query: String::new(),
            searchable,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_start: String::new(),
        };
        selector.refilter();
        selector
    }

    fn refilter(&mut self) {
        // The upstream selector uses fuzzyFilter over provider name/id/type
        // and method metadata. Preserve the original indexes while allowing
        // fuzzy_filter to rank the visible rows exactly like the shared TUI
        // selectors do.
        let indexed = self.options.iter().cloned().enumerate().collect();
        self.filtered_indices = fuzzy_filter(indexed, &self.query, |(_, option)| {
            format!(
                "{} {} {}",
                option.id,
                option.label,
                option.description.as_deref().unwrap_or_default()
            )
        })
        .into_iter()
        .map(|(index, _)| index)
        .collect();
        self.selected_index = self
            .selected_index
            .min(self.filtered_indices.len().saturating_sub(1));
    }

    fn selected_option(&self) -> Option<&pi_ai::auth::AuthSelectOption> {
        self.filtered_indices
            .get(self.selected_index)
            .and_then(|index| self.options.get(*index))
    }

    fn submit(&self) -> Option<String> {
        // Keep the old numeric input as a compatibility escape hatch for
        // scripted terminals, while the visible selector remains Pi-style
        // (there are no row numbers in the rendered component).
        if let Ok(number) = self.query.trim().parse::<usize>() {
            return self
                .options
                .get(number.saturating_sub(1))
                .map(|option| option.id.clone());
        }
        self.selected_option().map(|option| option.id.clone())
    }

    fn handle(&mut self, key: &TuiKey) -> AuthSurfaceAction {
        if matches!(key.base.as_str(), "release" | "repeat") {
            return AuthSurfaceAction::None;
        }
        if is_cancel(key) {
            return AuthSurfaceAction::Cancel;
        }
        if is_up(key) {
            self.selected_index = self.selected_index.saturating_sub(1);
            return AuthSurfaceAction::None;
        }
        if is_down(key) {
            if !self.filtered_indices.is_empty() {
                self.selected_index =
                    (self.selected_index + 1).min(self.filtered_indices.len().saturating_sub(1));
            }
            return AuthSurfaceAction::None;
        }
        if is_enter(key) {
            return self
                .submit()
                .map(AuthSurfaceAction::Submit)
                .unwrap_or(AuthSurfaceAction::None);
        }
        if key.base == "backspace" || (key.ctrl && key.base == "h") {
            if self.searchable || !self.query.is_empty() {
                self.query.pop();
                self.refilter();
            }
            return AuthSurfaceAction::None;
        }
        if !self.searchable
            && !key.base.is_empty()
            && key.base.chars().all(|character| character.is_ascii_digit())
        {
            self.query.push_str(&key.base);
            return AuthSurfaceAction::None;
        }
        if self.searchable && is_printable(key) {
            self.query.push_str(&key.base);
            self.refilter();
        }
        AuthSurfaceAction::None
    }

    /// Consume a bracketed-paste event before `parse_key` sees it. The
    /// terminal buffer normally emits a complete marker-wrapped event, but
    /// keeping the accumulated state here also handles a marker or payload
    /// split across reads. This mirrors the Input behavior used by upstream
    /// OAuthSelectorComponent.
    fn handle_raw(&mut self, raw: &str) -> Option<AuthSurfaceAction> {
        let mut pending = std::mem::take(&mut self.pending_start);
        pending.push_str(raw);

        loop {
            if self.paste_mode {
                self.paste_buffer.push_str(&pending);
                let Some(end) = self.paste_buffer.find(BRACKETED_PASTE_END) else {
                    return Some(AuthSurfaceAction::None);
                };
                let pasted = self.paste_buffer[..end].to_string();
                pending = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_string();
                self.paste_buffer.clear();
                self.paste_mode = false;
                self.handle_paste(&pasted);
                if pending.is_empty() {
                    return Some(AuthSurfaceAction::None);
                }
                continue;
            }

            let Some(start) = pending.find(BRACKETED_PASTE_START) else {
                if self.pending_start.is_empty() && pending == "\x1b" {
                    // TerminalBackend resolves a standalone ESC before this
                    // surface receives it. Do not hold it as a possible paste
                    // prefix or Esc cancellation would be lost.
                    return None;
                }
                let keep = partial_marker_suffix(&pending, BRACKETED_PASTE_START);
                let ordinary_end = pending.len() - keep.len();
                let ordinary = &pending[..ordinary_end];
                if !ordinary.is_empty() {
                    let action = self.handle_plain_burst(ordinary);
                    self.pending_start = keep.to_string();
                    return Some(action);
                }
                self.pending_start = keep.to_string();
                // Keep a marker prefix in this component rather than allowing
                // the ESC to become an accidental cancellation. Named keys
                // and ordinary single keys are still parsed by the surface
                // owner below.
                return if self.pending_start.is_empty() {
                    None
                } else {
                    Some(AuthSurfaceAction::None)
                };
            };
            if start > 0 {
                let prefix = pending[..start].to_string();
                let action = self.handle_plain_burst(&prefix);
                if action != AuthSurfaceAction::None {
                    return Some(action);
                }
            }
            self.paste_mode = true;
            self.paste_buffer.clear();
            pending = pending[start + BRACKETED_PASTE_START.len()..].to_string();
        }
    }

    fn handle_plain_burst(&mut self, text: &str) -> AuthSurfaceAction {
        if text.is_empty() || is_named_key_text(text) || text.chars().any(char::is_control) {
            return self.handle(&parse_key(text));
        }
        for (start, end) in pi_tui::grapheme_boundaries(text) {
            let action = self.handle(&parse_key(&text[start..end]));
            if action != AuthSurfaceAction::None {
                return action;
            }
        }
        AuthSurfaceAction::None
    }

    fn handle_paste(&mut self, pasted: &str) {
        let clean = clean_paste(pasted);
        if self.searchable {
            self.query.push_str(&clean);
            self.refilter();
        } else if !clean.is_empty() && clean.chars().all(|character| character.is_ascii_digit()) {
            // Keep the compatibility path used by scripted auth-method
            // prompts; upstream's visible selector remains row-number free.
            self.query.push_str(&clean);
        }
    }

    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(MIN_WIDTH);
        let mut lines = vec![
            border(width),
            t::fg(
                "accent",
                t::bold(truncate_to_width(&self.title, width, "…")),
            ),
        ];
        if self.searchable {
            lines.push(String::new());
            lines.push(truncate_to_width(&format!("> {}", self.query), width, "…"));
        }
        lines.push(String::new());

        let max_visible = 8;
        let start = self
            .selected_index
            .saturating_sub(max_visible / 2)
            .min(self.filtered_indices.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered_indices.len());
        for (visible_index, option_index) in self.filtered_indices[start..end].iter().enumerate() {
            let absolute_index = start + visible_index;
            let option = &self.options[*option_index];
            let prefix = if absolute_index == self.selected_index {
                t::fg("accent", "→ ")
            } else {
                "  ".to_string()
            };
            let label = if absolute_index == self.selected_index {
                t::fg("accent", option.label.clone())
            } else {
                option.label.clone()
            };
            let mut line = format!("{prefix}{label}");
            if let Some(description) = &option.description {
                line.push_str("  ");
                line.push_str(&t::fg("muted", format!("• {description}")));
            }
            lines.push(truncate_to_width(&line, width, "…"));
        }
        if self.filtered_indices.is_empty() {
            let empty = if self.options.is_empty() {
                "No providers available"
            } else {
                "No matching providers"
            };
            lines.push(t::fg("muted", format!("  {empty}")));
        } else if start > 0 || end < self.filtered_indices.len() {
            lines.push(t::fg(
                "muted",
                format!(
                    "  ({}/{})",
                    self.selected_index + 1,
                    self.filtered_indices.len()
                ),
            ));
        }
        lines.push(String::new());
        lines.push(t::dim("↑↓ navigate  Enter select  Esc cancel"));
        lines.push(border(width));
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width, "…"))
            .collect()
    }
}

struct DialogPrompt {
    message: String,
    placeholder: Option<String>,
    secret: bool,
    input: Input,
}

struct LoginDialog {
    title: String,
    details: Vec<String>,
    prompt: Option<DialogPrompt>,
    paste_mode: bool,
    paste_buffer: String,
    pending_start: String,
}

impl LoginDialog {
    fn new(title: String) -> Self {
        Self {
            title,
            details: Vec::new(),
            prompt: None,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_start: String::new(),
        }
    }

    fn set_prompt(&mut self, prompt: &AuthPrompt) {
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_start.clear();
        let (message, placeholder, secret) = match prompt {
            AuthPrompt::Text {
                message,
                placeholder,
            }
            | AuthPrompt::ManualCode {
                message,
                placeholder,
            } => (message.clone(), placeholder.clone(), false),
            AuthPrompt::Secret {
                message,
                placeholder,
            } => (message.clone(), placeholder.clone(), true),
            AuthPrompt::Select { .. } => return,
        };
        self.prompt = Some(DialogPrompt {
            message,
            placeholder,
            secret,
            input: Input::new(""),
        });
    }

    fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        self.details.clear();
        self.prompt = None;
        self.details
            .push(format!("Open this URL to sign in: {}", hyperlink(url, url)));
        self.details.push(hyperlink("Ctrl+click to open", url));
        if let Some(instructions) = instructions.filter(|text| !text.trim().is_empty()) {
            self.details.push(String::new());
            self.details.push(t::fg("warning", instructions));
        }
    }

    fn show_device_code(&mut self, verification_uri: &str, user_code: &str) {
        self.details.clear();
        self.prompt = None;
        self.details.push(format!(
            "Open this URL in a browser: {}",
            hyperlink(verification_uri, verification_uri)
        ));
        self.details
            .push(hyperlink("Ctrl+click to open", verification_uri));
        self.details.push(String::new());
        self.details
            .push(t::fg("warning", format!("Enter code: {user_code}")));
        self.details.push(t::dim("Waiting for authentication..."));
        self.details.push("(Esc to cancel)".to_string());
    }

    fn show_info(&mut self, message: &str, links: &[pi_ai::auth::AuthInfoLink]) {
        self.details.push(String::new());
        self.details.push(message.to_string());
        for link in links {
            let label = link.label.as_deref().unwrap_or(link.url.as_str());
            self.details
                .push(hyperlink(&format!("{label}: {}", link.url), &link.url));
        }
    }

    fn show_progress(&mut self, message: &str) {
        self.details.push(t::dim(message));
    }

    fn show_waiting(&mut self, message: &str) {
        self.details.push(String::new());
        self.details.push(t::dim(message));
        self.details.push("(Esc to cancel)".to_string());
    }

    fn submit(&mut self) -> AuthSurfaceAction {
        let Some(prompt) = self.prompt.take() else {
            return AuthSurfaceAction::None;
        };
        let value = prompt.input.get_value().to_string();
        let visible = if prompt.secret {
            "•".repeat(value.chars().count())
        } else {
            value.clone()
        };
        self.details.push(format!("> {visible}"));
        AuthSurfaceAction::Submit(value)
    }

    /// Handle the marker-wrapped paste event emitted by the terminal input
    /// buffer. Upstream's `Input` consumes paste data before key parsing, so
    /// API keys and manual codes must not be sent through `parse_key` as one
    /// giant escape sequence.
    fn handle_raw(&mut self, raw: &str) -> Option<AuthSurfaceAction> {
        let mut pending = std::mem::take(&mut self.pending_start);
        pending.push_str(raw);

        loop {
            if self.paste_mode {
                self.paste_buffer.push_str(&pending);
                let Some(end) = self.paste_buffer.find(BRACKETED_PASTE_END) else {
                    return Some(AuthSurfaceAction::None);
                };
                let pasted = self.paste_buffer[..end].to_string();
                pending = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_string();
                self.paste_buffer.clear();
                self.paste_mode = false;
                self.handle_paste(&pasted);
                if pending.is_empty() {
                    return Some(AuthSurfaceAction::None);
                }
                continue;
            }

            let Some(start) = pending.find(BRACKETED_PASTE_START) else {
                if self.pending_start.is_empty() && pending == "\x1b" {
                    // See the selector path above: a standalone Escape is a
                    // completed control key, not an incomplete paste marker.
                    return None;
                }
                let keep = partial_marker_suffix(&pending, BRACKETED_PASTE_START);
                let ordinary_end = pending.len() - keep.len();
                let ordinary = &pending[..ordinary_end];
                if !ordinary.is_empty() {
                    let action = self.handle_plain_burst(ordinary);
                    self.pending_start = keep.to_string();
                    return Some(action);
                }
                self.pending_start = keep.to_string();
                return if self.pending_start.is_empty() {
                    None
                } else {
                    Some(AuthSurfaceAction::None)
                };
            };
            if start > 0 {
                let prefix = pending[..start].to_string();
                let action = self.handle_plain_burst(&prefix);
                if action != AuthSurfaceAction::None {
                    return Some(action);
                }
            }
            self.paste_mode = true;
            self.paste_buffer.clear();
            pending = pending[start + BRACKETED_PASTE_START.len()..].to_string();
        }
    }

    fn handle_plain_burst(&mut self, text: &str) -> AuthSurfaceAction {
        if text.is_empty() || is_named_key_text(text) || text.chars().any(char::is_control) {
            return self.handle(&parse_key(text));
        }
        for (start, end) in pi_tui::grapheme_boundaries(text) {
            let action = self.handle(&parse_key(&text[start..end]));
            if action != AuthSurfaceAction::None {
                return action;
            }
        }
        AuthSurfaceAction::None
    }

    fn handle_paste(&mut self, pasted: &str) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        // Let Input own insertion/undo/grapheme semantics. Wrapping the
        // already-delimited payload avoids the single-key parser dropping a
        // control-bearing or multi-grapheme API key.
        prompt.input.handle_raw_input(&format!(
            "{BRACKETED_PASTE_START}{pasted}{BRACKETED_PASTE_END}"
        ));
    }

    fn handle(&mut self, key: &TuiKey) -> AuthSurfaceAction {
        if is_cancel(key) {
            return AuthSurfaceAction::Cancel;
        }
        if is_enter(key) {
            return self.submit();
        }
        let Some(prompt) = self.prompt.as_mut() else {
            return AuthSurfaceAction::None;
        };
        if key.base == "backspace" || (key.ctrl && key.base == "h") {
            // Ctrl+H is a legacy alias for Backspace; the shared Input owns
            // grapheme-safe deletion and cursor movement for the actual edit.
            if key.ctrl {
                prompt.input.handle_input(&TuiKey::simple("backspace"));
            } else {
                prompt.input.handle_input(key);
            }
        } else if is_printable(key) {
            prompt.input.handle_input(key);
        }
        AuthSurfaceAction::None
    }

    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(MIN_WIDTH);
        let mut lines = vec![
            border(width),
            t::fg(
                "accent",
                t::bold(truncate_to_width(&self.title, width, "…")),
            ),
        ];
        for detail in &self.details {
            lines.extend(wrap_line(detail, width));
        }
        if let Some(prompt) = &self.prompt {
            lines.push(String::new());
            lines.push(truncate_to_width(&prompt.message, width, "…"));
            if let Some(placeholder) = &prompt.placeholder {
                lines.push(t::dim(format!("e.g., {placeholder}")));
            }
            let visible = if prompt.secret {
                "•".repeat(prompt.input.get_value().chars().count())
            } else {
                prompt.input.get_value().to_string()
            };
            lines.push(truncate_to_width(&format!("> {visible}"), width, "…"));
            lines.push(t::dim("(Esc to cancel, Enter to submit)"));
        } else if !self.details.is_empty() {
            lines.push(String::new());
            lines.push(t::dim("(Esc to cancel)"));
        }
        lines.push(border(width));
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width, "…"))
            .collect()
    }
}

/// Shared state for the blocking auth interaction and the scene renderer.
/// Keeping this as a component makes the presentation reusable when the
/// interactive loop can mount the auth surface directly in the editor slot.
pub struct AuthSurfaceState {
    surface: AuthSurface,
    rendered_lines: usize,
    context_row: Option<String>,
    dialog_title: String,
}

enum AuthSurface {
    Selector(AuthSelector),
    Dialog(LoginDialog),
}

impl AuthSurfaceState {
    pub fn selector(
        title: impl Into<String>,
        options: Vec<pi_ai::auth::AuthSelectOption>,
        searchable: bool,
    ) -> Self {
        Self {
            surface: AuthSurface::Selector(AuthSelector::new(title.into(), options, searchable)),
            rendered_lines: 0,
            context_row: None,
            dialog_title: "Login".to_string(),
        }
    }

    pub fn dialog(title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            surface: AuthSurface::Dialog(LoginDialog::new(title.clone())),
            rendered_lines: 0,
            context_row: None,
            dialog_title: title,
        }
    }

    /// Retain the submitted slash-completion row above the auth surface. The
    /// official editor leaves that row in the transcript while the selector
    /// replaces the composer slot.
    pub fn set_context_row(&mut self, row: impl Into<String>) {
        self.context_row = Some(row.into());
    }

    pub fn set_selector(
        &mut self,
        title: impl Into<String>,
        options: Vec<pi_ai::auth::AuthSelectOption>,
        searchable: bool,
    ) {
        self.surface = AuthSurface::Selector(AuthSelector::new(title.into(), options, searchable));
    }

    pub fn set_dialog(&mut self, title: impl Into<String>) {
        let title = title.into();
        self.dialog_title = title.clone();
        self.surface = AuthSurface::Dialog(LoginDialog::new(title));
    }

    pub fn set_prompt(&mut self, prompt: &AuthPrompt) {
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.set_prompt(prompt);
        } else {
            self.surface = AuthSurface::Dialog(LoginDialog::new(self.dialog_title.clone()));
            if let AuthSurface::Dialog(dialog) = &mut self.surface {
                dialog.set_prompt(prompt);
            }
        }
    }

    pub fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        self.ensure_dialog();
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.show_auth(url, instructions);
        }
    }

    pub fn show_device_code(&mut self, verification_uri: &str, user_code: &str) {
        self.ensure_dialog();
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.show_device_code(verification_uri, user_code);
        }
    }

    pub fn show_info(&mut self, message: &str, links: &[pi_ai::auth::AuthInfoLink]) {
        self.ensure_dialog();
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.show_info(message, links);
        }
    }

    pub fn show_progress(&mut self, message: &str) {
        self.ensure_dialog();
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.show_progress(message);
        }
    }

    pub fn show_waiting(&mut self, message: &str) {
        self.ensure_dialog();
        if let AuthSurface::Dialog(dialog) = &mut self.surface {
            dialog.show_waiting(message);
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> AuthSurfaceAction {
        match &mut self.surface {
            AuthSurface::Selector(selector) => selector.handle(key),
            AuthSurface::Dialog(dialog) => dialog.handle(key),
        }
    }

    /// Dispatch a raw terminal event, consuming bracketed paste before the
    /// ordinary key parser. This is used by blocking provider-auth prompts;
    /// regular selectors continue to use the normal key path.
    pub fn handle_raw(&mut self, raw: &str) -> AuthSurfaceAction {
        let action = match &mut self.surface {
            AuthSurface::Selector(selector) => selector.handle_raw(raw),
            AuthSurface::Dialog(dialog) => dialog.handle_raw(raw),
        };
        if let Some(action) = action {
            return action;
        }
        self.handle(&parse_key(raw))
    }

    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let mut lines = self
            .context_row
            .as_deref()
            .map(|row| vec![truncate_to_width(row, width.max(MIN_WIDTH), "…")])
            .unwrap_or_default();
        let surface_lines = match &self.surface {
            AuthSurface::Selector(selector) => selector.render(width),
            AuthSurface::Dialog(dialog) => dialog.render(width),
        };
        lines.extend(surface_lines);
        lines
    }

    pub fn rendered_lines(&self) -> usize {
        self.rendered_lines
    }

    pub fn set_rendered_lines(&mut self, lines: usize) {
        self.rendered_lines = lines;
    }

    fn ensure_dialog(&mut self) {
        if !matches!(self.surface, AuthSurface::Dialog(_)) {
            self.surface = AuthSurface::Dialog(LoginDialog::new(self.dialog_title.clone()));
        }
    }
}

impl Component for AuthSurfaceState {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_lines(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

fn border(width: usize) -> String {
    "─".repeat(width.max(MIN_WIDTH))
}

fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x07{text}\x1b]8;;\x07")
}

fn wrap_line(text: &str, width: usize) -> Vec<String> {
    pi_tui::utils::wrap_text_with_ansi(text, width.max(1))
}

fn is_cancel(key: &TuiKey) -> bool {
    key.base == "esc" || key.base == "escape" || (key.ctrl && key.base == "c")
}

fn is_enter(key: &TuiKey) -> bool {
    key.base == "enter" && !key.ctrl && !key.alt
}

fn is_up(key: &TuiKey) -> bool {
    key.base == "up" && !key.ctrl && !key.alt
}

fn is_down(key: &TuiKey) -> bool {
    key.base == "down" && !key.ctrl && !key.alt
}

fn is_printable(key: &TuiKey) -> bool {
    !key.ctrl
        && !key.alt
        && !key.super_key
        && !key.base.is_empty()
        && key.base.chars().all(|character| !character.is_control())
}

fn clean_paste(pasted: &str) -> String {
    // Match the shared Input's single-line paste normalization used by the
    // upstream auth selectors/dialog: line breaks do not submit implicitly,
    // while tabs retain their visual indentation as spaces.
    pasted
        .replace("\r\n", "")
        .replace(['\r', '\n'], "")
        .replace('\t', "    ")
}

fn partial_marker_suffix<'a>(text: &'a str, marker: &str) -> &'a str {
    let max = text.len().min(marker.len().saturating_sub(1));
    for length in (1..=max).rev() {
        if text.ends_with(&marker[..length]) {
            return &text[text.len() - length..];
        }
    }
    ""
}

fn is_named_key_text(text: &str) -> bool {
    matches!(
        text,
        "enter"
            | "esc"
            | "escape"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "backspace"
            | "delete"
            | "tab"
            | "shift+tab"
            | "release"
            | "repeat"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn option(id: &str) -> pi_ai::auth::AuthSelectOption {
        pi_ai::auth::AuthSelectOption {
            id: id.to_string(),
            label: id.to_string(),
            description: None,
        }
    }

    #[test]
    fn selector_moves_one_row_per_arrow_press() {
        let mut surface = AuthSurfaceState::selector(
            "Select provider to configure:",
            vec![option("one"), option("two"), option("three")],
            true,
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("down")),
            AuthSurfaceAction::None
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("enter")),
            AuthSurfaceAction::Submit("two".into())
        );
    }

    #[test]
    fn non_searchable_selector_keeps_numeric_script_compatibility() {
        let mut surface = AuthSurfaceState::selector(
            "Select authentication method:",
            vec![option("one"), option("two")],
            false,
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("2")),
            AuthSurfaceAction::None
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("enter")),
            AuthSurfaceAction::Submit("two".into())
        );
    }

    #[test]
    fn kitty_release_is_not_a_component_action() {
        let mut surface = AuthSurfaceState::selector(
            "Select provider to configure:",
            vec![option("one"), option("two")],
            true,
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("down")),
            AuthSurfaceAction::None
        );
        // The terminal loop filters CSI-u releases before reaching this
        // component; this assertion documents that a release-shaped key is
        // not itself a navigation command.
        assert_eq!(
            surface.handle(&TuiKey::simple("release")),
            AuthSurfaceAction::None
        );
        assert_eq!(
            surface.handle(&TuiKey::simple("enter")),
            AuthSurfaceAction::Submit("two".into())
        );
    }

    #[test]
    fn dialog_escape_cancels_without_submitting_input() {
        let mut surface = AuthSurfaceState::dialog("Login to Example");
        surface.set_prompt(&AuthPrompt::ManualCode {
            message: "Paste code".into(),
            placeholder: None,
        });
        assert_eq!(
            surface.handle(&TuiKey::simple("esc")),
            AuthSurfaceAction::Cancel
        );
    }

    #[test]
    fn provider_selector_uses_fuzzy_search_and_bracketed_paste() {
        let mut surface = AuthSurfaceState::selector(
            "Select provider to configure:",
            vec![option("unrelated"), option("qwen-token-plan")],
            true,
        );

        surface.handle_raw("\x1b[200~qtp\x1b[201~");
        let rendered = surface.render_lines(80).join("\n");
        assert!(rendered.contains("qwen-token-plan"));
        assert!(!rendered.contains("unrelated"));
        assert_eq!(
            surface.handle(&TuiKey::simple("enter")),
            AuthSurfaceAction::Submit("qwen-token-plan".to_string())
        );
    }

    #[test]
    fn secret_prompt_preserves_pasted_value_but_only_renders_mask() {
        let mut surface = AuthSurfaceState::dialog("Login to Qwen Token Plan");
        surface.set_prompt(&AuthPrompt::Secret {
            message: "Enter Qwen Token Plan API key".to_string(),
            placeholder: None,
        });
        let fixture_key = "fixture-api-key-7f3a";
        surface.handle_raw(&format!(
            "{BRACKETED_PASTE_START}{fixture_key}{BRACKETED_PASTE_END}"
        ));
        let rendered = surface.render_lines(80).join("\n");
        assert!(!rendered.contains(fixture_key));
        assert!(rendered.contains(&"•".repeat(fixture_key.chars().count())));
        assert_eq!(
            surface.handle_raw("\r"),
            AuthSurfaceAction::Submit(fixture_key.to_string())
        );
    }

    #[test]
    fn split_paste_markers_and_coalesced_secret_text_are_lossless() {
        let mut surface = AuthSurfaceState::dialog("Login to Qwen Token Plan");
        surface.set_prompt(&AuthPrompt::Secret {
            message: "Enter Qwen Token Plan API key".to_string(),
            placeholder: None,
        });

        assert_eq!(surface.handle_raw("\x1b["), AuthSurfaceAction::None);
        assert_eq!(surface.handle_raw("200~qwen-"), AuthSurfaceAction::None);
        assert_eq!(
            surface.handle_raw("paste\x1b[201~"),
            AuthSurfaceAction::None
        );
        let rendered = surface.render_lines(80).join("\n");
        assert!(rendered.contains(&"•".repeat("qwen-paste".chars().count())));
        assert!(!rendered.contains("qwen-paste"));
        assert_eq!(
            surface.handle_raw("enter"),
            AuthSurfaceAction::Submit("qwen-paste".to_string())
        );
    }

    #[test]
    fn coalesced_selector_query_keeps_all_printable_graphemes() {
        let mut surface = AuthSurfaceState::selector(
            "Select provider to configure:",
            vec![option("qwen-token-plan"), option("unrelated")],
            true,
        );
        assert_eq!(surface.handle_raw("qwen"), AuthSurfaceAction::None);
        let rendered = surface.render_lines(80).join("\n");
        assert!(rendered.contains("> qwen"));
        assert!(rendered.contains("qwen-token-plan"));
        assert!(!rendered.contains("unrelated"));
    }
}
