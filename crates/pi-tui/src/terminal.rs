//! Terminal backend — crossterm raw-mode + alt-screen event loop. Produces
//! the pi key-string surface for the TUI tree to dispatch.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

#[cfg(not(unix))]
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::event::{KeyCode, KeyModifiers};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::keys::set_kitty_protocol_active;
use crate::stdin_buffer::{StdinBuffer, DEFAULT_ESCAPE_TIMEOUT_MS, DEFAULT_SEQUENCE_TIMEOUT_MS};
use crate::terminal_image::{
    get_capabilities, is_cell_size_response, parse_cell_size_response, set_cell_dimensions,
};

pub const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
pub const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
pub const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
pub const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
pub const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
pub const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
pub const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
pub const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
pub const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const CLEAR_SCREEN_HOME: &str = "\x1b[2J\x1b[H";
pub const BEGIN_SYNC_UPDATE: &str = "\x1b[?2026h";
pub const END_SYNC_UPDATE: &str = "\x1b[?2026l";
pub const KITTY_KEYBOARD_PROTOCOL_QUERY: &str = "\x1b[>7u\x1b[?u\x1b[c";
pub const DISABLE_KITTY_KEYBOARD_PROTOCOL: &str = "\x1b[<u";
pub const ENABLE_MODIFY_OTHER_KEYS: &str = "\x1b[>4;2m";
pub const DISABLE_MODIFY_OTHER_KEYS: &str = "\x1b[>4;0m";
pub const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";
pub const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0\x07";
pub const TERMINAL_PROGRESS_KEEPALIVE_MS: u64 = 1_000;
pub const NATIVE_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";
const KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);

type OutputCapture = Arc<StdMutex<Option<Vec<u8>>>>;
type OutputLock = Arc<StdMutex<()>>;

/// Own the repeating OSC 9;4 progress notification without borrowing the
/// terminal backend across a thread. The condition variable makes shutdown
/// immediate instead of forcing terminal teardown to wait for a sleeping
/// timer, while the shared capture buffer keeps timer output testable.
struct ProgressKeepalive {
    stop: Arc<AtomicBool>,
    wake: Arc<(StdMutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProgressKeepalive {
    fn start(output_capture: OutputCapture, output_lock: OutputLock) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((StdMutex::new(false), Condvar::new()));
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);
        let thread = std::thread::spawn(move || loop {
            let (lock, condition) = &*thread_wake;
            let guard = match lock.lock() {
                Ok(guard) => guard,
                Err(_) => break,
            };
            let (guard, timeout) = match condition
                .wait_timeout(guard, Duration::from_millis(TERMINAL_PROGRESS_KEEPALIVE_MS))
            {
                Ok(result) => result,
                Err(_) => break,
            };
            drop(guard);
            if thread_stop.load(Ordering::Acquire) {
                break;
            }
            if timeout.timed_out() {
                write_progress_bytes(
                    &output_capture,
                    &output_lock,
                    TERMINAL_PROGRESS_ACTIVE_SEQUENCE,
                );
            }
        });
        Self {
            stop,
            wake,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_progress_bytes(output_capture: &OutputCapture, output_lock: &OutputLock, sequence: &str) {
    use std::io::Write;
    let _output_guard = output_lock.lock().ok();
    if let Ok(mut capture) = output_capture.lock() {
        if let Some(bytes) = capture.as_mut() {
            bytes.extend_from_slice(sequence.as_bytes());
        }
    }
    let mut output = std::io::stdout().lock();
    let _ = output.write_all(sequence.as_bytes());
    let _ = output.flush();
}

/// Complete, deterministic terminal-mode sequences used by overlays and
/// tests. tmux/screen-like transports use button-motion reporting because
/// all-motion mode is not reliably forwarded through the multiplexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSequences {
    pub enter: String,
    pub suspend: String,
    pub restore: String,
    pub cleanup: String,
}

pub fn alt_screen_sequences(through_multiplexer: bool) -> AltScreenSequences {
    let mouse = if through_multiplexer {
        ENABLE_BUTTON_MOTION_MOUSE
    } else {
        ENABLE_ALL_MOTION_MOUSE
    };
    AltScreenSequences {
        enter: format!(
            "{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse}{CLEAR_SCREEN_HOME}{HIDE_CURSOR}"
        ),
        suspend: format!("{DISABLE_MOUSE}{ENABLE_AUTOWRAP}{EXIT_ALT_SCREEN}{SHOW_CURSOR}"),
        restore: format!(
            "{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse}{CLEAR_SCREEN_HOME}{HIDE_CURSOR}"
        ),
        cleanup: format!("{BEGIN_SYNC_UPDATE}{DISABLE_MOUSE}{ENABLE_AUTOWRAP}{END_SYNC_UPDATE}"),
    }
}

/// A terminal event handed to the interactive loop.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    Key(String),
    Resize(u16, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolResponse {
    KittyFlags(u32),
    DeviceAttributes,
}

pub struct TerminalBackend {
    width: u16,
    height: u16,
    raw: bool,
    virtual_terminal: bool,
    alt_screen: bool,
    alt_screen_depth: usize,
    screen_epoch: u64,
    #[cfg(unix)]
    stdin: std::io::Stdin,
    #[cfg(unix)]
    stdin_buffer: StdinBuffer,
    #[cfg(unix)]
    pending_input: VecDeque<String>,
    #[cfg(unix)]
    pending_input_tail_plain: bool,
    #[cfg(unix)]
    incomplete_input_since: Option<std::time::Instant>,
    #[cfg(unix)]
    escape_sequence_timeout: Duration,
    #[cfg(unix)]
    sequence_timeout: Duration,
    #[cfg(unix)]
    last_resize_check: Option<Instant>,
    #[cfg(unix)]
    pending_utf8: Vec<u8>,
    #[cfg(unix)]
    stdin_eof: bool,
    keyboard_protocol_pushed: bool,
    kitty_protocol_active: bool,
    modify_other_keys_active: bool,
    keyboard_protocol_buffer: String,
    keyboard_protocol_buffer_since: Option<Instant>,
    output_capture: OutputCapture,
    output_lock: OutputLock,
    progress_active: bool,
    progress_keepalive: Option<ProgressKeepalive>,
}

impl TerminalBackend {
    pub fn new() -> Self {
        let (width, height) = terminal_dimensions();
        Self {
            width,
            height,
            raw: false,
            virtual_terminal: false,
            alt_screen: false,
            alt_screen_depth: 0,
            screen_epoch: 0,
            #[cfg(unix)]
            stdin: std::io::stdin(),
            #[cfg(unix)]
            stdin_buffer: StdinBuffer::new(),
            #[cfg(unix)]
            pending_input: VecDeque::new(),
            #[cfg(unix)]
            pending_input_tail_plain: false,
            #[cfg(unix)]
            incomplete_input_since: None,
            #[cfg(unix)]
            escape_sequence_timeout: resolve_escape_timeout(),
            #[cfg(unix)]
            sequence_timeout: Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS),
            #[cfg(unix)]
            last_resize_check: None,
            #[cfg(unix)]
            pending_utf8: Vec::new(),
            #[cfg(unix)]
            stdin_eof: false,
            keyboard_protocol_pushed: false,
            kitty_protocol_active: false,
            modify_other_keys_active: false,
            keyboard_protocol_buffer: String::new(),
            keyboard_protocol_buffer_since: None,
            output_capture: Arc::new(StdMutex::new(None)),
            output_lock: Arc::new(StdMutex::new(())),
            progress_active: false,
            progress_keepalive: None,
        }
    }

    /// Construct a backend with deterministic dimensions without querying the
    /// process terminal. This is also useful to embedders that own their
    /// terminal-size source (PTYs and GUI terminal adapters).
    pub fn new_with_size(width: u16, height: u16) -> Self {
        let mut terminal = Self::new();
        terminal.width = width.max(1);
        terminal.height = height.max(1);
        terminal.virtual_terminal = true;
        terminal
    }

    pub fn width(&self) -> usize {
        self.width as usize
    }
    pub fn height(&self) -> usize {
        self.height as usize
    }

    /// Update dimensions after a resize notification. Returns whether the
    /// dimensions changed; zero-sized terminal reports are clamped to one
    /// cell so renderers never emit invalid `CSI 0;...` positions.
    pub fn set_size(&mut self, width: u16, height: u16) -> bool {
        let width = width.max(1);
        let height = height.max(1);
        let changed = self.width != width || self.height != height;
        self.width = width;
        self.height = height;
        changed
    }

    /// Capture subsequent terminal writes for deterministic protocol and TUI
    /// tests. Capturing is opt-in so normal interactive sessions do not retain
    /// the complete output stream in memory.
    pub fn begin_output_capture(&mut self) {
        if let Ok(mut capture) = self.output_capture.lock() {
            *capture = Some(Vec::new());
        }
    }

    /// Return and clear captured terminal bytes.
    pub fn take_output_capture(&mut self) -> Vec<u8> {
        self.output_capture
            .lock()
            .ok()
            .and_then(|mut capture| capture.take())
            .unwrap_or_default()
    }

    pub fn move_by(&mut self, lines: isize) {
        if lines > 0 {
            self.write_raw(&format!("\x1b[{}B", lines));
        } else if lines < 0 {
            self.write_raw(&format!("\x1b[{}A", lines.unsigned_abs()));
        }
    }

    pub fn hide_cursor(&mut self) {
        self.write_raw(HIDE_CURSOR);
    }

    pub fn show_cursor(&mut self) {
        self.write_raw(SHOW_CURSOR);
    }

    pub fn clear_line(&mut self) {
        // Match ProcessTerminal.clearLine(): erase from the cursor through the
        // end of the current line, preserving content to the left.
        self.write_raw("\x1b[K");
    }

    pub fn clear_from_cursor(&mut self) {
        // CSI J defaults to erase-from-cursor-to-end-of-screen (mode 0), and
        // matches ProcessTerminal.clearFromCursor() byte-for-byte.
        self.write_raw("\x1b[J");
    }

    pub fn clear_screen(&mut self) {
        self.write_raw(CLEAR_SCREEN_HOME);
    }

    pub fn set_title(&mut self, title: &str) {
        // OSC title payloads cannot contain a raw BEL/ESC terminator. Strip
        // them rather than allowing a title call to inject terminal state.
        let safe_title = title.replace(['\x07', '\x1b'], "");
        self.write_raw(&format!("\x1b]0;{safe_title}\x07"));
    }

    pub fn set_progress(&mut self, active: bool) {
        if active {
            self.progress_active = true;
            self.write_raw(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            if self.progress_keepalive.is_none() {
                self.progress_keepalive = Some(ProgressKeepalive::start(
                    Arc::clone(&self.output_capture),
                    Arc::clone(&self.output_lock),
                ));
            }
        } else {
            self.stop_progress_keepalive();
            self.progress_active = false;
            self.write_raw(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }

    pub fn progress_active(&self) -> bool {
        self.progress_active
    }

    pub fn enter_raw(&mut self) -> std::io::Result<()> {
        self.enter_raw_with_alt_screen(true)
    }

    /// Enter raw mode while optionally retaining the user's main screen.
    /// Regular TUI mode owns raw input and redraws in scrollback; fullscreen
    /// mode additionally enters the alternate screen.
    pub fn enter_raw_with_alt_screen(&mut self, use_alt_screen: bool) -> std::io::Result<()> {
        let was_raw = self.raw;
        #[cfg(unix)]
        if !was_raw {
            self.reset_input_state();
        }
        if !self.raw {
            if !self.virtual_terminal {
                crossterm::terminal::enable_raw_mode()?;
            }
            self.raw = true;
        }
        if use_alt_screen && !self.alt_screen {
            self.enter_alt_screen();
        }
        if !was_raw {
            self.write_raw(ENABLE_BRACKETED_PASTE);
            self.keyboard_protocol_pushed = true;
            self.keyboard_protocol_buffer.clear();
            self.keyboard_protocol_buffer_since = None;
            self.write_raw(KITTY_KEYBOARD_PROTOCOL_QUERY);
        }
        Ok(())
    }

    pub fn leave_raw(&mut self) -> std::io::Result<()> {
        if self.progress_active {
            self.stop_progress_keepalive();
            self.progress_active = false;
            self.write_raw(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        } else {
            self.stop_progress_keepalive();
        }
        while self.alt_screen {
            self.leave_alt_screen();
        }
        self.disable_keyboard_protocols();
        #[cfg(unix)]
        self.reset_input_state();
        // The fullscreen path restores the cursor while leaving the
        // alternate screen. Regular mode never enters that screen, so it
        // needs the same explicit terminal-state restoration on exit.
        self.write_raw(SHOW_CURSOR);
        self.write_raw(DISABLE_BRACKETED_PASTE);
        if self.raw {
            self.raw = false;
            if self.virtual_terminal {
                Ok(())
            } else {
                crossterm::terminal::disable_raw_mode()
            }
        } else {
            Ok(())
        }
    }

    /// Enter the alternate screen while preserving raw-mode ownership.
    /// Overlays use this instead of re-enabling raw mode, so nested UI
    /// surfaces can suspend and restore the previous screen safely.
    pub fn enter_alt_screen(&mut self) {
        if self.alt_screen {
            self.alt_screen_depth = self.alt_screen_depth.saturating_add(1);
            return;
        }
        let sequences = alt_screen_sequences(terminal_is_multiplexed());
        self.write_raw(&sequences.enter);
        self.alt_screen = true;
        self.alt_screen_depth = 1;
        self.screen_epoch = self.screen_epoch.wrapping_add(1);
        let _ = self.flush();
    }

    /// Leave the alternate screen and restore the cursor.
    pub fn leave_alt_screen(&mut self) {
        if !self.alt_screen {
            return;
        }
        if self.alt_screen_depth > 1 {
            self.alt_screen_depth -= 1;
            return;
        }
        let sequences = alt_screen_sequences(terminal_is_multiplexed());
        self.write_raw(&format!("{}{}", sequences.cleanup, sequences.suspend));
        self.alt_screen = false;
        self.alt_screen_depth = 0;
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

    /// Whether the terminal confirmed Kitty keyboard protocol support.
    pub fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }

    /// Whether the terminal is currently using xterm modifyOtherKeys as the
    /// fallback keyboard protocol.
    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_active
    }

    /// Whether the Unix stdin descriptor has reached EOF. The interactive
    /// event pump uses this to stop its reader task instead of repeatedly
    /// polling an already-closed PTY.
    #[cfg(unix)]
    pub fn stdin_eof(&self) -> bool {
        self.stdin_eof
    }

    #[cfg(not(unix))]
    pub fn stdin_eof(&self) -> bool {
        false
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
        let _output_guard = self.output_lock.lock().ok();
        if let Ok(mut capture) = self.output_capture.lock() {
            if let Some(bytes) = capture.as_mut() {
                bytes.extend_from_slice(s.as_bytes());
            }
        }
        let mut out = std::io::stdout().lock();
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
    /// sizing dimensions when it contains positive values. Transition
    /// responses with a zero dimension are also consumed, but leave the
    /// previous geometry intact.
    pub fn consume_cell_size_response(&mut self, data: &str) -> bool {
        if !is_cell_size_response(data) {
            return false;
        }
        if let Some((width, height)) = parse_cell_size_response(data) {
            set_cell_dimensions(width, height);
        }
        true
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        let _output_guard = self.output_lock.lock().ok();
        std::io::stdout().lock().flush()
    }

    #[cfg(unix)]
    fn reset_input_state(&mut self) {
        self.stdin_buffer.destroy();
        self.pending_input.clear();
        self.pending_input_tail_plain = false;
        self.incomplete_input_since = None;
        self.pending_utf8.clear();
        self.stdin_eof = false;
    }

    fn stop_progress_keepalive(&mut self) {
        if let Some(mut keepalive) = self.progress_keepalive.take() {
            keepalive.stop();
        }
    }

    /// Read the next terminal event from raw stdin, preserving complete escape
    /// sequences for the TUI key parser. Reading raw bytes here is important:
    /// crossterm's event parser rejects terminal replies such as the cell-size
    /// response (`CSI 6;height;width t`) before the TUI can consume them.
    #[cfg(unix)]
    pub fn next_event(&mut self) -> std::io::Result<TerminalEvent> {
        loop {
            self.flush_expired_keyboard_protocol_buffer();
            if let Some(input) = self.pop_pending_input() {
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
                    if let Some(input) = self.pop_pending_input() {
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

    /// Try to read one terminal event without waiting for stdin.
    ///
    /// The interactive agent keeps a background reader alive for the whole
    /// session. A blocking `next_event` call would hold the shared terminal
    /// mutex while its poll timeout expires, making the renderer wait behind
    /// an idle input reader. Keeping the poll non-blocking here lets the
    /// reader release the mutex between checks, so redraws and key handling
    /// stay responsive even when no input is arriving.
    #[cfg(unix)]
    pub fn try_next_event(&mut self) -> std::io::Result<Option<TerminalEvent>> {
        loop {
            self.flush_expired_keyboard_protocol_buffer();
            if let Some(input) = self.pop_pending_input() {
                return Ok(Some(TerminalEvent::Key(input)));
            }

            if let Some(event) = self.poll_resize()? {
                return Ok(Some(event));
            }

            if self.stdin_eof {
                return Ok(Some(TerminalEvent::Key(String::new())));
            }

            // `next_event` waits for the configured escape timeout so a
            // partial sequence can be distinguished from a standalone Esc.
            // The worker must not sleep while holding the terminal mutex, so
            // flush that same partial sequence when its deadline has elapsed
            // and otherwise return control to the caller immediately.
            if let Some(started) = self.incomplete_input_since {
                if started.elapsed() >= self.pending_input_timeout() {
                    let flushed = self.stdin_buffer.flush();
                    self.enqueue_sequences(flushed);
                    self.incomplete_input_since = None;
                    if let Some(input) = self.pop_pending_input() {
                        return Ok(Some(TerminalEvent::Key(input)));
                    }
                }
            }

            if !self.wait_for_stdin(Duration::ZERO)? {
                return Ok(None);
            }

            let mut bytes = [0_u8; 4096];
            let read = self.stdin.read(&mut bytes)?;
            if read == 0 {
                self.stdin_eof = true;
                return Ok(Some(TerminalEvent::Key(String::new())));
            }
            self.process_input_bytes(&bytes[..read]);
        }
    }

    /// Return the maximum time the background reader should wait before
    /// re-entering [`Self::try_next_event`]. Ordinary input is delivered by
    /// fd readiness, while the bounded wake-up preserves resize detection and
    /// lets a partial escape sequence expire without a fixed per-key sleep.
    #[cfg(unix)]
    pub fn input_wait_timeout_hint(&self) -> Duration {
        const INPUT_WAKE_INTERVAL: Duration = Duration::from_millis(16);
        self.incomplete_input_since
            .map(|started| {
                self.pending_input_timeout()
                    .saturating_sub(started.elapsed())
                    .min(INPUT_WAKE_INTERVAL)
            })
            .unwrap_or(INPUT_WAKE_INTERVAL)
    }

    /// Return stdin's stable file descriptor so an owner can wait for input
    /// without holding the shared terminal mutex. The descriptor is copied;
    /// no terminal state is borrowed after this call returns.
    #[cfg(unix)]
    pub fn stdin_fd(&self) -> i32 {
        self.stdin.as_raw_fd()
    }

    /// Poll a terminal input descriptor without touching parser state. This
    /// is intentionally separate from [`Self::try_next_event`]: the
    /// interactive reader can block here while the renderer acquires the
    /// terminal mutex to write the current frame.
    #[cfg(unix)]
    pub fn poll_input_fd(fd: i32, timeout: Option<Duration>) -> io::Result<bool> {
        let timeout_ms = timeout
            .map(|duration| duration.as_millis().min(i32::MAX as u128) as i32)
            .unwrap_or(-1);
        let mut descriptor = libc::pollfd {
            fd,
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
                self.set_size(w, h);
                Ok(TerminalEvent::Resize(self.width, self.height))
            }
            _ => Ok(TerminalEvent::Key(String::new())),
        }
    }

    /// Portable fallback for platforms without the raw Unix stdin path.
    /// Crossterm owns the platform event wait there, so retain its existing
    /// behavior behind the same API used by the interactive reader.
    #[cfg(not(unix))]
    pub fn try_next_event(&mut self) -> std::io::Result<Option<TerminalEvent>> {
        self.next_event().map(Some)
    }

    #[cfg(unix)]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn process_input_bytes(&mut self, bytes: &[u8]) {
        self.pending_utf8.extend_from_slice(bytes);

        loop {
            match std::str::from_utf8(&self.pending_utf8) {
                Ok(_) => {
                    // Move the already-owned UTF-8 buffer into a String for
                    // processing instead of cloning every terminal read.
                    // Reuse its allocation for the next read afterwards.
                    let bytes = std::mem::take(&mut self.pending_utf8);
                    let text = String::from_utf8(bytes).expect("validated UTF-8 input");
                    self.process_input_text(&text);
                    self.pending_utf8 = text.into_bytes();
                    self.pending_utf8.clear();
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
        let sequences = self.stdin_buffer.process(text);
        for sequence in sequences {
            // Keyboard-protocol replies always begin with ESC. Avoid probing
            // the protocol parser and allocating a temporary combined string
            // for ordinary printable input, which is the hot path while
            // typing.
            if self.keyboard_protocol_buffer.is_empty() && !sequence.starts_with('\x1b') {
                self.enqueue_sequence(sequence);
                continue;
            }
            if self.handle_keyboard_protocol_sequence(&sequence) {
                continue;
            }
            self.enqueue_sequence(sequence);
        }
        if self.stdin_buffer.get_buffer().is_empty() {
            self.incomplete_input_since = None;
        } else {
            // StdinBuffer's upstream timer is restarted after every fragment.
            // Resetting the owner deadline here prevents a slow but valid
            // escape sequence from being flushed from its first fragment.
            self.incomplete_input_since = Some(std::time::Instant::now());
        }
    }

    fn handle_keyboard_protocol_sequence(&mut self, sequence: &str) -> bool {
        if self.keyboard_protocol_buffer.is_empty() {
            if let Some(response) = parse_keyboard_protocol_response(sequence) {
                self.apply_keyboard_protocol_response(response);
                return true;
            }
            if is_keyboard_protocol_response_prefix(sequence) {
                self.keyboard_protocol_buffer.push_str(sequence);
                self.keyboard_protocol_buffer_since = Some(Instant::now());
                return true;
            }
            return false;
        }

        let mut combined =
            String::with_capacity(self.keyboard_protocol_buffer.len() + sequence.len());
        combined.push_str(&self.keyboard_protocol_buffer);
        combined.push_str(sequence);
        if let Some(response) = parse_keyboard_protocol_response(&combined) {
            self.keyboard_protocol_buffer.clear();
            self.keyboard_protocol_buffer_since = None;
            self.apply_keyboard_protocol_response(response);
            return true;
        }
        if is_keyboard_protocol_response_prefix(&combined) {
            self.keyboard_protocol_buffer = combined;
            self.keyboard_protocol_buffer_since = Some(Instant::now());
            return true;
        }

        if !self.keyboard_protocol_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.keyboard_protocol_buffer);
            self.keyboard_protocol_buffer_since = None;
            self.enqueue_non_printable_sequence(buffered);
        }
        false
    }

    fn apply_keyboard_protocol_response(&mut self, response: KeyboardProtocolResponse) {
        match response {
            KeyboardProtocolResponse::KittyFlags(flags) if flags != 0 => {
                self.disable_modify_other_keys();
                self.kitty_protocol_active = true;
                set_kitty_protocol_active(true);
            }
            KeyboardProtocolResponse::KittyFlags(_)
            | KeyboardProtocolResponse::DeviceAttributes => {
                if !self.kitty_protocol_active {
                    self.enable_modify_other_keys();
                }
            }
        }
    }

    fn flush_expired_keyboard_protocol_buffer(&mut self) {
        let expired = self.keyboard_protocol_buffer_since.is_some_and(|started| {
            started.elapsed() >= KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT
        });
        if !expired {
            return;
        }
        let buffered = std::mem::take(&mut self.keyboard_protocol_buffer);
        self.keyboard_protocol_buffer_since = None;
        if !buffered.is_empty() {
            self.enqueue_non_printable_sequence(buffered);
        }
    }

    fn enable_modify_other_keys(&mut self) {
        if self.kitty_protocol_active || self.modify_other_keys_active {
            return;
        }
        self.write_raw(ENABLE_MODIFY_OTHER_KEYS);
        self.modify_other_keys_active = true;
    }

    fn disable_modify_other_keys(&mut self) {
        if !self.modify_other_keys_active {
            return;
        }
        self.write_raw(DISABLE_MODIFY_OTHER_KEYS);
        self.modify_other_keys_active = false;
    }

    fn disable_keyboard_protocols(&mut self) {
        self.keyboard_protocol_buffer.clear();
        self.keyboard_protocol_buffer_since = None;
        if self.keyboard_protocol_pushed || self.kitty_protocol_active {
            self.write_raw(DISABLE_KITTY_KEYBOARD_PROTOCOL);
        }
        self.keyboard_protocol_pushed = false;
        self.kitty_protocol_active = false;
        set_kitty_protocol_active(false);
        self.disable_modify_other_keys();
    }

    #[cfg(unix)]
    fn enqueue_sequences(&mut self, sequences: Vec<String>) {
        for sequence in sequences {
            self.enqueue_sequence(sequence);
        }
    }

    #[cfg(unix)]
    fn enqueue_sequence(&mut self, sequence: String) {
        // A single stdin read commonly contains a fast typed/pasted burst of
        // printable characters. Keep control/escape sequences as individual
        // key events, but coalesce adjacent printable data so the interactive
        // renderer does not redraw once per byte. Track the tail kind instead
        // of rescanning the already-coalesced string on every character.
        let plain = is_plain_printable_input(&sequence);
        if plain && self.pending_input_tail_plain {
            if let Some(previous) = self.pending_input.back_mut() {
                previous.push_str(&sequence);
                return;
            }
        }
        self.pending_input.push_back(sequence);
        self.pending_input_tail_plain = plain;
    }

    #[cfg(unix)]
    fn enqueue_non_printable_sequence(&mut self, sequence: String) {
        self.pending_input.push_back(sequence);
        self.pending_input_tail_plain = false;
    }

    #[cfg(unix)]
    fn pop_pending_input(&mut self) -> Option<String> {
        let input = self.pending_input.pop_front();
        if self.pending_input.is_empty() {
            self.pending_input_tail_plain = false;
        }
        input
    }

    #[cfg(unix)]
    fn input_wait_timeout(&self) -> Duration {
        let Some(started) = self.incomplete_input_since else {
            return self.sequence_timeout;
        };
        self.pending_input_timeout()
            .saturating_sub(started.elapsed())
    }

    #[cfg(unix)]
    fn pending_input_timeout(&self) -> Duration {
        if self.stdin_buffer.is_lone_escape_pending() {
            self.escape_sequence_timeout
        } else {
            self.sequence_timeout
        }
    }

    #[cfg(unix)]
    fn wait_for_stdin(&self, timeout: Duration) -> io::Result<bool> {
        if self.stdin_eof {
            return Ok(false);
        }
        Self::poll_input_fd(self.stdin.as_raw_fd(), Some(timeout))
    }

    #[cfg(unix)]
    fn poll_resize(&mut self) -> io::Result<Option<TerminalEvent>> {
        // `new_with_size` deliberately represents a caller-owned/virtual
        // terminal. Querying the host tty in that mode creates spurious
        // resize events and needlessly wakes the input loop every 16 ms.
        if self.virtual_terminal {
            return Ok(None);
        }
        const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(16);
        let now = Instant::now();
        if self
            .last_resize_check
            .is_some_and(|last| now.duration_since(last) < RESIZE_POLL_INTERVAL)
        {
            return Ok(None);
        }
        self.last_resize_check = Some(now);
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

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        let was_active = self.progress_active;
        self.stop_progress_keepalive();
        if was_active {
            self.progress_active = false;
            self.write_raw(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }
}

#[cfg(unix)]
fn is_plain_printable_input(input: &str) -> bool {
    !input.is_empty()
        && !input.contains('\x1b')
        && input.chars().all(|character| !character.is_control())
}

pub fn parse_keyboard_protocol_response(sequence: &str) -> Option<KeyboardProtocolResponse> {
    let body = sequence.strip_prefix("\x1b[?")?;
    if let Some(flags) = body.strip_suffix('u') {
        if !flags.is_empty() && flags.chars().all(|character| character.is_ascii_digit()) {
            return flags.parse().ok().map(KeyboardProtocolResponse::KittyFlags);
        }
    }
    if let Some(attributes) = body.strip_suffix('c') {
        if attributes
            .chars()
            .all(|character| character.is_ascii_digit() || character == ';')
        {
            return Some(KeyboardProtocolResponse::DeviceAttributes);
        }
    }
    None
}

fn is_keyboard_protocol_response_prefix(sequence: &str) -> bool {
    if sequence == "\x1b[" {
        return true;
    }
    let Some(body) = sequence.strip_prefix("\x1b[?") else {
        return false;
    };
    body.chars()
        .all(|character| character.is_ascii_digit() || character == ';')
}

impl Default for TerminalBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn terminal_is_multiplexed() -> bool {
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    is_terminal_multiplexed(
        &term,
        std::env::var_os("TMUX").is_some(),
        std::env::var_os("ZELLIJ").is_some(),
        std::env::var_os("STY").is_some(),
    )
}

fn is_terminal_multiplexed(term: &str, tmux: bool, zellij: bool, screen: bool) -> bool {
    // Match TuiAltScreen's mouse-mode detection: STY identifies GNU Screen
    // even when TERM was inherited from the outer terminal.
    tmux || zellij || screen || term.starts_with("tmux") || term.starts_with("screen")
}

#[cfg(unix)]
fn resolve_escape_timeout() -> Duration {
    let configured = std::env::var("PI_TUI_ESC_TIMEOUT").ok();
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    Duration::from_millis(resolve_escape_timeout_ms(configured.as_deref(), ssh))
}

/// Resolve the escape reassembly timeout without reading process-global
/// environment state. This pure form makes SSH and invalid-configuration
/// behavior testable and is used by the real backend above.
pub fn resolve_escape_timeout_ms(configured: Option<&str>, ssh: bool) -> u64 {
    if let Some(value) = configured.and_then(|value| value.parse::<u64>().ok()) {
        if value > 0 {
            return value;
        }
    }
    if ssh {
        100
    } else {
        DEFAULT_ESCAPE_TIMEOUT_MS
    }
}

pub fn normalize_native_shift_enter_input(
    data: &str,
    detect_native_shift_enter: bool,
    shift_pressed: bool,
) -> String {
    if detect_native_shift_enter && shift_pressed && data == "\r" {
        NATIVE_SHIFT_ENTER_SEQUENCE.to_string()
    } else {
        data.to_string()
    }
}

pub fn normalize_apple_terminal_input(
    data: &str,
    is_apple_terminal: bool,
    shift_pressed: bool,
) -> String {
    normalize_native_shift_enter_input(data, is_apple_terminal, shift_pressed)
}

fn terminal_dimensions() -> (u16, u16) {
    if let Ok((width, height)) = crossterm::terminal::size() {
        if width > 0 && height > 0 {
            return (width, height);
        }
    }
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(80);
    let height = std::env::var("LINES")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(24);
    (width, height)
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
        if !(base.len() == 1 && base.chars().next().unwrap_or(' ').is_ascii_alphabetic())
            && !parts.contains(&"ctrl".to_string())
            && !parts.contains(&"alt".to_string())
        {
            parts.push("shift".to_string());
        }
    }
    parts.push(base);
    parts.join("+")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn escape_timeout_and_shift_enter_normalization_match_upstream() {
        assert_eq!(resolve_escape_timeout_ms(Some("80"), false), 80);
        assert_eq!(resolve_escape_timeout_ms(Some("80"), true), 80);
        assert_eq!(resolve_escape_timeout_ms(Some("abc"), false), 10);
        assert_eq!(resolve_escape_timeout_ms(Some("0"), false), 10);
        assert_eq!(resolve_escape_timeout_ms(Some("-5"), false), 10);
        assert_eq!(resolve_escape_timeout_ms(None, true), 100);
        assert_eq!(resolve_escape_timeout_ms(None, false), 10);
        assert_eq!(
            normalize_native_shift_enter_input("\r", true, true),
            NATIVE_SHIFT_ENTER_SEQUENCE
        );
        assert_eq!(normalize_native_shift_enter_input("\r", true, false), "\r");
        assert_eq!(normalize_apple_terminal_input("a", true, true), "a");
    }

    #[test]
    fn terminal_output_protocol_helpers_are_real_sequences() {
        let mut terminal = TerminalBackend::new_with_size(10, 4);
        terminal.begin_output_capture();
        terminal.set_progress(false);
        terminal.set_progress(true);
        terminal.move_by(-2);
        terminal.move_by(3);
        terminal.clear_line();
        terminal.clear_from_cursor();
        terminal.clear_screen();
        terminal.set_title("safe\x07\x1b[31m");
        let output = String::from_utf8(terminal.take_output_capture()).unwrap();
        assert!(output.contains(TERMINAL_PROGRESS_CLEAR_SEQUENCE));
        assert!(output.contains(TERMINAL_PROGRESS_ACTIVE_SEQUENCE));
        assert!(output.contains("\x1b[2A"));
        assert!(output.contains("\x1b[3B"));
        assert!(output.contains("\x1b]0;safe[31m\x07"));
        assert!(!output.contains("\x07\x1b[31m"));
        assert!(terminal.progress_active());
    }

    #[test]
    fn clear_line_erases_from_cursor_to_end_like_upstream() {
        let mut terminal = TerminalBackend::new_with_size(10, 4);
        terminal.begin_output_capture();
        terminal.clear_line();
        assert_eq!(
            String::from_utf8(terminal.take_output_capture()).unwrap(),
            "\x1b[K"
        );
    }

    #[test]
    fn clear_from_cursor_erases_to_screen_end_like_upstream() {
        let mut terminal = TerminalBackend::new_with_size(10, 4);
        terminal.begin_output_capture();
        terminal.clear_from_cursor();
        assert_eq!(
            String::from_utf8(terminal.take_output_capture()).unwrap(),
            "\x1b[J"
        );
    }

    #[test]
    fn terminal_progress_keepalive_repeats_and_clears() {
        let mut terminal = TerminalBackend::new_with_size(10, 4);
        terminal.begin_output_capture();
        terminal.set_progress(true);
        std::thread::sleep(Duration::from_millis(TERMINAL_PROGRESS_KEEPALIVE_MS + 100));
        terminal.set_progress(false);
        let output = String::from_utf8(terminal.take_output_capture()).unwrap();
        assert!(
            output.matches(TERMINAL_PROGRESS_ACTIVE_SEQUENCE).count() >= 2,
            "keepalive did not emit a second progress notification: {output:?}"
        );
        assert!(output.ends_with(TERMINAL_PROGRESS_CLEAR_SEQUENCE));
        assert!(!terminal.progress_active());
        println!("TERMINAL_PROGRESS_TESTS_OK");
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
    fn nested_alt_screen_save_restore_has_one_terminal_transition() {
        let mut terminal = TerminalBackend::new();
        terminal.enter_alt_screen();
        terminal.enter_alt_screen();
        assert!(terminal.is_alt_screen());
        terminal.leave_alt_screen();
        assert!(terminal.is_alt_screen());
        terminal.leave_alt_screen();
        assert!(!terminal.is_alt_screen());
        assert_eq!(terminal.screen_epoch(), 2);
    }

    #[test]
    fn alt_screen_sequences_use_conservative_tmux_mouse_mode() {
        let direct = alt_screen_sequences(false);
        let tmux = alt_screen_sequences(true);
        assert!(direct.enter.contains(ENABLE_ALL_MOTION_MOUSE));
        assert!(tmux.enter.contains(ENABLE_BUTTON_MOTION_MOUSE));
        assert!(!tmux.enter.contains(ENABLE_ALL_MOTION_MOUSE));
        assert!(tmux.cleanup.starts_with(BEGIN_SYNC_UPDATE));
        assert!(tmux.cleanup.ends_with(END_SYNC_UPDATE));
        assert!(tmux.suspend.contains(EXIT_ALT_SCREEN));
        assert!(tmux.suspend.contains(SHOW_CURSOR));
    }

    #[test]
    fn screen_environment_selects_conservative_mouse_mode_without_term_hint() {
        assert!(is_terminal_multiplexed(
            "xterm-256color",
            false,
            false,
            true
        ));
        assert!(is_terminal_multiplexed(
            "screen-256color",
            false,
            false,
            false
        ));
        assert!(is_terminal_multiplexed(
            "tmux-256color",
            false,
            false,
            false
        ));
        assert!(!is_terminal_multiplexed(
            "xterm-256color",
            false,
            false,
            false
        ));
    }

    #[test]
    fn cell_size_query_and_response_update_image_dimensions() {
        let original = crate::terminal_image::get_cell_dimensions();
        let mut terminal = TerminalBackend::new();
        assert!(terminal.consume_cell_size_response("\x1b[6;0;9t"));
        assert_eq!(crate::terminal_image::get_cell_dimensions(), original);
        assert!(terminal.consume_cell_size_response("\x1b[6;18;9t"));
        assert_eq!(crate::terminal_image::get_cell_dimensions(), (9, 18));
        crate::terminal_image::set_cell_dimensions(original.0, original.1);
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

    #[test]
    fn parses_keyboard_protocol_replies_and_falls_back_to_modify_other_keys() {
        assert_eq!(
            parse_keyboard_protocol_response("\x1b[?1u"),
            Some(KeyboardProtocolResponse::KittyFlags(1))
        );
        assert_eq!(
            parse_keyboard_protocol_response("\x1b[?1;2c"),
            Some(KeyboardProtocolResponse::DeviceAttributes)
        );
        assert!(is_keyboard_protocol_response_prefix("\x1b[?1;"));
        assert!(!is_keyboard_protocol_response_prefix("\x1b[6;18;9t"));

        let mut terminal = TerminalBackend::new();
        assert!(terminal.handle_keyboard_protocol_sequence("\x1b[?1;2c"));
        assert!(!terminal.kitty_protocol_active);
        assert!(terminal.modify_other_keys_active);
        terminal.handle_keyboard_protocol_sequence("\x1b[?1u");
        assert!(terminal.kitty_protocol_active);
        assert!(!terminal.modify_other_keys_active);
        terminal.disable_keyboard_protocols();
        assert!(!terminal.kitty_protocol_active);
        assert!(!terminal.modify_other_keys_active);
    }

    #[test]
    fn keyboard_protocol_fragments_and_nonresponses_are_replayed_in_order() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);
        assert!(terminal.handle_keyboard_protocol_sequence("\x1b[?7"));
        assert!(terminal.handle_keyboard_protocol_sequence("u"));
        assert!(terminal.kitty_protocol_active());

        let mut terminal = TerminalBackend::new_with_size(80, 24);
        assert!(terminal.handle_keyboard_protocol_sequence("\x1b["));
        assert!(!terminal.handle_keyboard_protocol_sequence("a"));
        assert_eq!(
            terminal.pending_input.pop_front(),
            Some("\x1b[".to_string())
        );
        assert!(!terminal.kitty_protocol_active());
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_keyboard_protocol_reply_is_flushed_as_input_after_timeout() {
        let mut terminal = TerminalBackend::new();
        assert!(terminal.handle_keyboard_protocol_sequence("\x1b[?"));
        terminal.keyboard_protocol_buffer_since =
            Some(Instant::now() - KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT);
        terminal.flush_expired_keyboard_protocol_buffer();
        assert_eq!(
            terminal.pending_input.pop_front(),
            Some("\x1b[?".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn coalesces_adjacent_printable_input_without_crossing_controls() {
        let mut terminal = TerminalBackend::new();
        terminal.enqueue_sequences(vec![
            "a".to_string(),
            "b".to_string(),
            "\r".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]);

        assert_eq!(
            std::mem::take(&mut terminal.pending_input)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["ab".to_string(), "\r".to_string(), "cd".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn coalesced_input_tail_state_handles_large_printable_bursts() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);
        terminal.enqueue_sequences((0..4_096).map(|_| "x".to_string()).collect());

        assert_eq!(terminal.pending_input.len(), 1);
        assert_eq!(terminal.pending_input.front().map(String::len), Some(4_096));
        assert!(terminal.pending_input_tail_plain);
    }

    #[cfg(unix)]
    #[test]
    fn partial_utf8_reads_wait_for_a_complete_codepoint_and_preserve_order() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);
        terminal.process_input_bytes(&[0xe2]);
        assert!(terminal.pending_input.is_empty());
        assert_eq!(terminal.pending_utf8, vec![0xe2]);

        terminal.process_input_bytes(&[0x82, 0xac, b'a']);
        assert_eq!(terminal.pop_pending_input(), Some("€a".to_string()));
        assert!(terminal.pending_utf8.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn virtual_terminal_skips_physical_resize_poll_and_wakeup() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);
        assert!(terminal.last_resize_check.is_none());
        assert_eq!(terminal.poll_resize().unwrap(), None);
        assert!(terminal.last_resize_check.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn input_deadlines_match_upstream_and_reset_after_each_fragment() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);

        terminal.process_input_text("\x1b");
        let escape_remaining = terminal.input_wait_timeout();
        assert!(escape_remaining > Duration::ZERO);
        assert!(escape_remaining <= Duration::from_millis(DEFAULT_ESCAPE_TIMEOUT_MS));

        terminal.stdin_buffer.flush();
        terminal.incomplete_input_since = None;
        terminal.process_input_text("\x1b[");
        let sequence_remaining = terminal.input_wait_timeout();
        assert!(sequence_remaining > Duration::from_millis(40));
        assert!(sequence_remaining <= Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS));

        // A later fragment restarts the upstream timeout instead of inheriting
        // the age of the first fragment.
        terminal.incomplete_input_since =
            Some(Instant::now() - Duration::from_millis(DEFAULT_SEQUENCE_TIMEOUT_MS - 5));
        terminal.process_input_text("1");
        assert!(terminal.input_wait_timeout() > Duration::from_millis(40));
    }

    #[cfg(unix)]
    #[test]
    fn leaving_raw_cancels_queued_input_and_partial_utf8() {
        let mut terminal = TerminalBackend::new_with_size(80, 24);
        terminal.process_input_text("\x1b[");
        terminal.pending_input.push_back("queued".to_string());
        terminal.pending_utf8.extend_from_slice(&[0xf0, 0x9f]);
        terminal.stdin_eof = true;

        terminal.leave_raw().unwrap();

        assert!(terminal.stdin_buffer.get_buffer().is_empty());
        assert!(terminal.pending_input.is_empty());
        assert!(terminal.pending_utf8.is_empty());
        assert!(terminal.incomplete_input_since.is_none());
        assert!(!terminal.stdin_eof);
    }
}
