//! Terminal backend — crossterm raw-mode + alt-screen event loop. Produces
//! the pi key-string surface for the TUI tree to dispatch.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::time::Duration;

#[cfg(not(unix))]
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::event::{KeyCode, KeyModifiers};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::stdin_buffer::StdinBuffer;
use crate::terminal_image::{get_capabilities, parse_cell_size_response, set_cell_dimensions};

/// A terminal event handed to the interactive loop.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    Key(String),
    Resize(u16, u16),
}

pub struct TerminalBackend {
    width: u16,
    height: u16,
    raw: bool,
    alt_screen: bool,
    screen_epoch: u64,
    #[cfg(unix)]
    stdin: std::io::Stdin,
    #[cfg(unix)]
    stdin_buffer: StdinBuffer,
    #[cfg(unix)]
    pending_input: VecDeque<String>,
    #[cfg(unix)]
    incomplete_input_since: Option<std::time::Instant>,
    #[cfg(unix)]
    escape_sequence_timeout: Duration,
    #[cfg(unix)]
    pending_utf8: Vec<u8>,
    #[cfg(unix)]
    stdin_eof: bool,
}

impl TerminalBackend {
    pub fn new() -> Self {
        let (width, height) = match crossterm::terminal::size() {
            Ok((w, h)) => (w.max(1), h.max(1)),
            Err(_) => (80, 24),
        };
        Self {
            width,
            height,
            raw: false,
            alt_screen: false,
            screen_epoch: 0,
            #[cfg(unix)]
            stdin: std::io::stdin(),
            #[cfg(unix)]
            stdin_buffer: StdinBuffer::new(),
            #[cfg(unix)]
            pending_input: VecDeque::new(),
            #[cfg(unix)]
            incomplete_input_since: None,
            #[cfg(unix)]
            escape_sequence_timeout: resolve_escape_timeout(),
            #[cfg(unix)]
            pending_utf8: Vec::new(),
            #[cfg(unix)]
            stdin_eof: false,
        }
    }

    pub fn width(&self) -> usize {
        self.width as usize
    }
    pub fn height(&self) -> usize {
        self.height as usize
    }

    pub fn enter_raw(&mut self) -> std::io::Result<()> {
        if !self.raw {
            crossterm::terminal::enable_raw_mode()?;
            self.raw = true;
        }
        self.enter_alt_screen();
        Ok(())
    }

    pub fn leave_raw(&mut self) -> std::io::Result<()> {
        self.leave_alt_screen();
        if self.raw {
            self.raw = false;
            crossterm::terminal::disable_raw_mode()
        } else {
            Ok(())
        }
    }

    /// Enter the alternate screen while preserving raw-mode ownership.
    /// Overlays use this instead of re-enabling raw mode, so nested UI
    /// surfaces can suspend and restore the previous screen safely.
    pub fn enter_alt_screen(&mut self) {
        if self.alt_screen {
            return;
        }
        self.write_raw("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H");
        self.alt_screen = true;
        self.screen_epoch = self.screen_epoch.wrapping_add(1);
        let _ = self.flush();
    }

    /// Leave the alternate screen and restore the cursor.
    pub fn leave_alt_screen(&mut self) {
        if !self.alt_screen {
            return;
        }
        self.write_raw("\x1b[?25h\x1b[?1049l");
        self.alt_screen = false;
        self.screen_epoch = self.screen_epoch.wrapping_add(1);
        let _ = self.flush();
    }

    /// Temporarily reveal the user's main screen while keeping raw mode on.
    pub fn suspend_alt_screen(&mut self) {
        self.leave_alt_screen();
    }

    /// Restore a previously suspended alternate screen.
    pub fn resume_alt_screen(&mut self) {
        self.enter_alt_screen();
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }

    pub fn is_alt_screen(&self) -> bool {
        self.alt_screen
    }

    /// Monotonic token for alternate-screen transitions. Differential
    /// renderers use this to detect a screen that was temporarily replaced
    /// by an overlay or external prompt and force a complete redraw when it
    /// is restored.
    pub fn screen_epoch(&self) -> u64 {
        self.screen_epoch
    }

    pub fn write_raw(&mut self, s: &str) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    /// Ask an image-capable terminal for its cell dimensions in pixels.
    ///
    /// The response is `CSI 6;height;width t`; callers should pass raw input
    /// fragments to [`Self::consume_cell_size_response`] as they arrive.
    pub fn query_cell_size(&mut self) -> bool {
        if get_capabilities().images.is_none() {
            return false;
        }
        self.write_raw("\x1b[16t");
        true
    }

    /// Consume a terminal cell-size response and update the shared image
    /// sizing dimensions. Returns `true` only when the complete response was
    /// recognized and contained positive dimensions.
    pub fn consume_cell_size_response(&mut self, data: &str) -> bool {
        let Some((width, height)) = parse_cell_size_response(data) else {
            return false;
        };
        set_cell_dimensions(width, height);
        true
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        std::io::stdout().flush()
    }

    /// Read the next terminal event from raw stdin, preserving complete escape
    /// sequences for the TUI key parser. Reading raw bytes here is important:
    /// crossterm's event parser rejects terminal replies such as the cell-size
    /// response (`CSI 6;height;width t`) before the TUI can consume them.
    #[cfg(unix)]
    pub fn next_event(&mut self) -> std::io::Result<TerminalEvent> {
        loop {
            if let Some(input) = self.pending_input.pop_front() {
                return Ok(TerminalEvent::Key(input));
            }

            if let Some(event) = self.poll_resize()? {
                return Ok(event);
            }

            let timeout = self.input_wait_timeout();
            if !self.wait_for_stdin(timeout)? {
                if self.incomplete_input_since.is_some() {
                    let flushed = self.stdin_buffer.flush();
                    self.enqueue_sequences(flushed);
                    self.incomplete_input_since = None;
                    if let Some(input) = self.pending_input.pop_front() {
                        return Ok(TerminalEvent::Key(input));
                    }
                }
                return Ok(TerminalEvent::Key(String::new()));
            }

            let mut bytes = [0_u8; 4096];
            let read = self.stdin.read(&mut bytes)?;
            if read == 0 {
                self.stdin_eof = true;
                return Ok(TerminalEvent::Key(String::new()));
            }
            self.process_input_bytes(&bytes[..read]);
        }
    }

    /// Read the next terminal event through crossterm on platforms where the
    /// Unix file-descriptor path is unavailable. Unix uses the raw path above
    /// so terminal replies remain visible to the TUI.
    #[cfg(not(unix))]
    pub fn next_event(&mut self) -> std::io::Result<TerminalEvent> {
        if !event::poll(Duration::from_millis(50))? {
            return Ok(TerminalEvent::Key(String::new()));
        }
        match event::read()? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                ..
            }) => Ok(TerminalEvent::Key(key_string(code, modifiers))),
            Event::Key(_) => Ok(TerminalEvent::Key(String::new())),
            Event::Resize(w, h) => {
                self.width = w.max(1);
                self.height = h.max(1);
                Ok(TerminalEvent::Resize(self.width, self.height))
            }
            _ => Ok(TerminalEvent::Key(String::new())),
        }
    }

    #[cfg(unix)]
    fn process_input_bytes(&mut self, bytes: &[u8]) {
        self.pending_utf8.extend_from_slice(bytes);

        loop {
            match std::str::from_utf8(&self.pending_utf8) {
                Ok(text) => {
                    let text = text.to_string();
                    self.pending_utf8.clear();
                    self.process_input_text(&text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = String::from_utf8(self.pending_utf8[..valid].to_vec())
                            .expect("valid UTF-8 prefix");
                        self.pending_utf8.drain(..valid);
                        self.process_input_text(&text);
                        continue;
                    }

                    // A terminal normally emits UTF-8. If an invalid byte does
                    // arrive, consume it as a replacement character instead of
                    // leaving the reader permanently stuck on the same byte.
                    if error.error_len().is_some() {
                        self.pending_utf8.remove(0);
                        self.process_input_text("\u{fffd}");
                        continue;
                    }
                    break;
                }
            }
        }
    }

    #[cfg(unix)]
    fn process_input_text(&mut self, text: &str) {
        let had_incomplete_input = !self.stdin_buffer.get_buffer().is_empty();
        let sequences = self.stdin_buffer.process(text);
        self.enqueue_sequences(sequences);
        if self.stdin_buffer.get_buffer().is_empty() {
            self.incomplete_input_since = None;
        } else if !had_incomplete_input {
            self.incomplete_input_since = Some(std::time::Instant::now());
        }
    }

    #[cfg(unix)]
    fn enqueue_sequences(&mut self, sequences: Vec<String>) {
        self.pending_input.extend(sequences);
    }

    #[cfg(unix)]
    fn input_wait_timeout(&self) -> Duration {
        const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
        let Some(started) = self.incomplete_input_since else {
            return INPUT_POLL_TIMEOUT;
        };
        self.escape_sequence_timeout
            .saturating_sub(started.elapsed())
    }

    #[cfg(unix)]
    fn wait_for_stdin(&self, timeout: Duration) -> io::Result<bool> {
        if self.stdin_eof {
            return Ok(false);
        }

        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: self.stdin.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `descriptor` points to one initialized pollfd and lives
            // for the duration of the call.
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if result >= 0 {
                return Ok(result > 0
                    && (descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }

    #[cfg(unix)]
    fn poll_resize(&mut self) -> io::Result<Option<TerminalEvent>> {
        let Ok((width, height)) = crossterm::terminal::size() else {
            return Ok(None);
        };
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return Ok(None);
        }
        self.width = width;
        self.height = height;
        Ok(Some(TerminalEvent::Resize(width, height)))
    }
}

#[cfg(unix)]
fn resolve_escape_timeout() -> Duration {
    const DEFAULT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(10);
    const DEFAULT_SSH_ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

    if let Ok(value) = std::env::var("PI_TUI_ESC_TIMEOUT") {
        if let Ok(milliseconds) = value.parse::<u64>() {
            if milliseconds > 0 {
                return Duration::from_millis(milliseconds);
            }
        }
    }
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        DEFAULT_SSH_ESCAPE_TIMEOUT
    } else {
        DEFAULT_ESCAPE_TIMEOUT
    }
}

#[cfg_attr(unix, allow(dead_code))]
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
        assert_eq!(
            key_string(KeyCode::Char('c'), KeyModifiers::CONTROL),
            "ctrl+c"
        );
        assert_eq!(key_string(KeyCode::Char('a'), KeyModifiers::NONE), "a");
        assert_eq!(
            key_string(KeyCode::BackTab, KeyModifiers::NONE),
            "shift+tab"
        );
    }

    #[test]
    fn mode_state_is_idempotent_before_terminal_activation() {
        let mut terminal = TerminalBackend::new();
        assert!(!terminal.is_raw());
        assert!(!terminal.is_alt_screen());
        assert_eq!(terminal.screen_epoch(), 0);
        terminal.leave_alt_screen();
        terminal.suspend_alt_screen();
        terminal.resume_alt_screen();
        // The state-only transitions are harmless even when stdout is not a
        // terminal; the explicit leave restores the invariant for callers.
        assert!(terminal.is_alt_screen());
        assert_eq!(terminal.screen_epoch(), 1);
        terminal.leave_alt_screen();
        assert!(!terminal.is_alt_screen());
        assert_eq!(terminal.screen_epoch(), 2);
    }

    #[test]
    fn cell_size_query_and_response_update_image_dimensions() {
        let mut terminal = TerminalBackend::new();
        assert!(!terminal.consume_cell_size_response("\x1b[6;0;9t"));
        assert!(terminal.consume_cell_size_response("\x1b[6;18;9t"));
        assert_eq!(crate::terminal_image::get_cell_dimensions(), (9, 18));
    }

    #[cfg(unix)]
    #[test]
    fn raw_input_preserves_cell_response_and_following_key_order() {
        let mut terminal = TerminalBackend::new();
        terminal.process_input_text("\x1b[6;18;9tq");

        assert_eq!(
            terminal.pending_input.pop_front(),
            Some("\x1b[6;18;9t".to_string())
        );
        assert_eq!(terminal.pending_input.pop_front(), Some("q".to_string()));
    }
}
