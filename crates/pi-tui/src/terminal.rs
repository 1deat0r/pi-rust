//! Terminal backend — crossterm raw-mode + alt-screen event loop. Produces
//! the pi key-string surface for the TUI tree to dispatch.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A terminal event handed to the interactive loop.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    Key(String),
    Resize(u16, u16),
}

pub struct TerminalBackend {
    width: u16,
    height: u16,
}

impl TerminalBackend {
    pub fn new() -> Self {
        let (width, height) = match crossterm::terminal::size() {
            Ok((w, h)) => (w.max(1), h.max(1)),
            Err(_) => (80, 24),
        };
        Self { width, height }
    }

    pub fn width(&self) -> usize {
        self.width as usize
    }
    pub fn height(&self) -> usize {
        self.height as usize
    }

    pub fn enter_raw(&mut self) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        self.write_raw("\x1b[?1049h"); // alt screen
        self.write_raw("\x1b[?25l"); // hide cursor
        self.write_raw("\x1b[2J\x1b[H");
        let _ = self.flush();
        Ok(())
    }

    pub fn leave_raw(&mut self) -> std::io::Result<()> {
        self.write_raw("\x1b[?25h"); // show cursor
        self.write_raw("\x1b[?1049l"); // leave alt screen
        let _ = self.flush();
        crossterm::terminal::disable_raw_mode()
    }

    pub fn write_raw(&mut self, s: &str) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        std::io::stdout().flush()
    }

    /// Read the next terminal event (blocking), converting to the key
    /// string surface. Polls for resize.
    pub fn next_event(&mut self) -> std::io::Result<TerminalEvent> {
        if !event::poll(Duration::from_millis(50))? {
            return Ok(TerminalEvent::Key(String::new()));
        }
        match event::read()? {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                Ok(TerminalEvent::Key(key_string(code, modifiers)))
            }
            Event::Key(_) => Ok(TerminalEvent::Key(String::new())),
            Event::Resize(w, h) => {
                self.width = w.max(1);
                self.height = h.max(1);
                Ok(TerminalEvent::Resize(self.width, self.height))
            }
            _ => Ok(TerminalEvent::Key(String::new())),
        }
    }
}

fn key_string(code: KeyCode, modifiers: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "shift+tab".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => String::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) && !base.is_empty() {
        // Only annotate shift for non-character keys (upstream normalizes).
        if !(base.len() == 1 && base.chars().next().unwrap_or(' ').is_ascii_alphabetic()) {
            if !parts.contains(&"ctrl".to_string()) && !parts.contains(&"alt".to_string()) {
                parts.push("shift".to_string());
            }
        }
    }
    parts.push(base);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_strings() {
        assert_eq!(key_string(KeyCode::Enter, KeyModifiers::NONE), "enter");
        assert_eq!(key_string(KeyCode::Char('c'), KeyModifiers::CONTROL), "ctrl+c");
        assert_eq!(key_string(KeyCode::Char('a'), KeyModifiers::NONE), "a");
        assert_eq!(key_string(KeyCode::BackTab, KeyModifiers::NONE), "shift+tab");
    }
}
